// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Test} from "forge-std/Test.sol";
import {SolanaLightClient} from "../src/SolanaLightClient.sol";
import {SolanaControlled} from "../src/SolanaControlled.sol";
import {SolanaGreeter} from "../src/examples/SolanaGreeter.sol";
import {BindingVerifier} from "../src/mocks/BindingVerifier.sol";

/// 7.4: a contract a Solana key controls. Transactions are built here byte by byte (legacy
/// format, one Memo instruction) and wrapped in the proof's output layout; the stand-in
/// verifier binds the "proof" to that output like the real one would.
contract SolanaGreeterTest is Test {
    uint16 constant SHRED_VERSION = 28090;
    bytes32 constant SET_0 = 0xb1f25236519c10b064498b6507ed0b87b88ad4df728a0a348e9434769d7d3c11;
    bytes32 constant MEMO = 0x054a53522854cdd103cc2e4631a3f51792ac727cf1e50abc97828ac5994f3cc2;
    bytes32 constant VKEY = bytes32(uint256(0x000482e7));
    bytes32 constant CONTROLLER = keccak256("the controller's Solana key");
    bytes32 constant STRANGER = keccak256("someone else's Solana key");

    BindingVerifier verifier;
    SolanaLightClient client;
    SolanaGreeter greeter;

    function setUp() public {
        verifier = new BindingVerifier();
        client = new SolanaLightClient(verifier, VKEY, SHRED_VERSION, 432000, 0, SET_0);
        greeter = new SolanaGreeter(client, CONTROLLER);
    }

    function test_controllerChangesTheGreeting() public {
        assertEq(greeter.greeting(), "gm");
        _execute(_memoTx(CONTROLLER, 1, _memo(block.chainid, address(greeter), "1", "hello from solana")));
        assertEq(greeter.greeting(), "hello from solana");
        assertEq(greeter.nonce(), 1);
    }

    function test_memoPrefixIsWhatToolsBuild() public view {
        assertEq(
            greeter.memoPrefix(),
            string.concat("huginn:", vm.toString(block.chainid), ":", vm.toLowercase(vm.toString(address(greeter))), ":")
        );
    }

    function test_addressInAnyLetterCase() public {
        // vm.toString(address) is the checksummed (mixed-case) form explorers show.
        bytes memory memo = abi.encodePacked("huginn:", vm.toString(block.chainid), ":", vm.toString(address(greeter)), ":1:mixed case");
        _execute(_memoTx(CONTROLLER, 1, memo));
        assertEq(greeter.greeting(), "mixed case");
    }

    function test_strangerCannot() public {
        _expectRevert(_memoTx(STRANGER, 1, _memo(block.chainid, address(greeter), "1", "pwned")), abi.encodeWithSelector(SolanaControlled.NotSignedByController.selector));
    }

    function test_noReplay() public {
        bytes memory tx_ = _memoTx(CONTROLLER, 1, _memo(block.chainid, address(greeter), "1", "once"));
        _execute(tx_);
        _expectRevert(tx_, abi.encodeWithSelector(SolanaControlled.WrongNonce.selector, uint64(1), uint64(2)));
        // The same text again needs the next nonce.
        _execute(_memoTx(CONTROLLER, 2, _memo(block.chainid, address(greeter), "2", "once")));
        assertEq(greeter.nonce(), 2);
    }

    function test_commandsRunInOrder() public {
        bytes memory first = _memoTx(CONTROLLER, 1, _memo(block.chainid, address(greeter), "1", "A"));
        bytes memory second = _memoTx(CONTROLLER, 2, _memo(block.chainid, address(greeter), "2", "B"));
        // A relayer can't run the second before the first...
        _expectRevert(second, abi.encodeWithSelector(SolanaControlled.WrongNonce.selector, uint64(2), uint64(1)));
        _execute(first);
        _execute(second);
        // ...nor the first after the second.
        _expectRevert(first, abi.encodeWithSelector(SolanaControlled.WrongNonce.selector, uint64(1), uint64(3)));
        assertEq(greeter.greeting(), "B");
    }

    function test_emptyCommandCancels() public {
        bytes memory signed = _memoTx(CONTROLLER, 1, _memo(block.chainid, address(greeter), "1", "regretted"));
        _execute(_memoTx(CONTROLLER, 2, _memo(block.chainid, address(greeter), "1", "")));
        assertEq(greeter.greeting(), "gm");
        assertEq(greeter.nonce(), 1);
        _expectRevert(signed, abi.encodeWithSelector(SolanaControlled.WrongNonce.selector, uint64(1), uint64(2)));
    }

    function test_badNoncesRejected() public {
        string[5] memory bad = ["", "01", "x", "1x", "12345678901234567890"];
        for (uint256 i; i < bad.length; i++) {
            _expectRevert(_memoTx(CONTROLLER, 1, _memo(block.chainid, address(greeter), bad[i], "hi")), abi.encodeWithSelector(SolanaControlled.BadNonce.selector));
        }
        // No ':' after the nonce.
        bytes memory noColon = abi.encodePacked(greeter.memoPrefix(), "1");
        _expectRevert(_memoTx(CONTROLLER, 1, noColon), abi.encodeWithSelector(SolanaControlled.BadNonce.selector));
    }

    function test_memoForAnotherChainIsIgnored() public {
        _expectRevert(_memoTx(CONTROLLER, 1, _memo(8453, address(greeter), "1", "base mainnet only")), abi.encodeWithSelector(SolanaControlled.NoCommand.selector));
    }

    function test_memoForAnotherDeploymentIsIgnored() public {
        SolanaGreeter v2 = new SolanaGreeter(client, CONTROLLER);
        bytes memory forV1 = _memoTx(CONTROLLER, 1, _memo(block.chainid, address(greeter), "1", "v1 only"));
        _execute(forV1);
        bytes memory pv = _pv(forV1);
        bytes memory proof = verifier.proofFor(VKEY, pv);
        vm.expectRevert(SolanaControlled.NoCommand.selector);
        v2.execute(pv, proof);
        assertEq(v2.greeting(), "gm");
    }

    function test_forgedCommandRejected() public {
        bytes memory pv = _pv(_memoTx(CONTROLLER, 1, _memo(block.chainid, address(greeter), "1", "hello")));
        bytes memory proof = verifier.proofFor(VKEY, pv);
        pv[pv.length - 1] = "X"; // "hellX": not what the controller signed
        vm.expectRevert(BindingVerifier.InvalidProof.selector);
        greeter.execute(pv, proof);
    }

    function test_longCommand() public {
        // 200+ bytes: the data length takes two compact-u16 bytes.
        bytes memory long = new bytes(200);
        for (uint256 i; i < long.length; i++) {
            long[i] = "a";
        }
        _execute(_memoTx(CONTROLLER, 1, _memo(block.chainid, address(greeter), "1", string(long))));
        assertEq(bytes(greeter.greeting()).length, 200);
    }

    // ---- building blocks ----------------------------------------------------------------

    function _execute(bytes memory tx_) internal {
        bytes memory pv = _pv(tx_);
        greeter.execute(pv, verifier.proofFor(VKEY, pv));
    }

    function _expectRevert(bytes memory tx_, bytes memory err) internal {
        bytes memory pv = _pv(tx_);
        bytes memory proof = verifier.proofFor(VKEY, pv);
        vm.expectRevert(err);
        greeter.execute(pv, proof);
    }

    /// `huginn:<chain>:<address, lowercase>:<nonce>:<command>`
    function _memo(uint256 chain, address to, string memory nonce, string memory command) internal pure returns (bytes memory) {
        return abi.encodePacked("huginn:", vm.toString(chain), ":", vm.toLowercase(vm.toString(to)), ":", nonce, ":", command);
    }

    /// Legacy transaction: 1 signature, keys [signer, Memo program], one Memo instruction.
    /// `sigSeed` only changes the signature bytes.
    function _memoTx(bytes32 signer, uint8 sigSeed, bytes memory memo) internal pure returns (bytes memory) {
        bytes memory sig = abi.encodePacked(bytes32(uint256(sigSeed)), bytes32(0));
        return bytes.concat(
            hex"01", sig, // signatures
            hex"010001", // header: 1 signer, 0 readonly signed, 1 readonly unsigned
            hex"02", signer, MEMO, // account keys
            keccak256("blockhash"),
            hex"01", hex"01", hex"0100", // 1 instruction: program 1 (Memo), accounts [0]
            _compactU16(memo.length), memo
        );
    }

    function _compactU16(uint256 n) internal pure returns (bytes memory) {
        if (n < 0x80) return abi.encodePacked(uint8(n));
        if (n < 0x4000) return abi.encodePacked(uint8(n & 0x7f) | 0x80, uint8(n >> 7));
        return abi.encodePacked(uint8(n & 0x7f) | 0x80, uint8((n >> 7) & 0x7f) | 0x80, uint8(n >> 14));
    }

    /// The proof's output for `tx_` in slot 50,000 of the local chain (epoch 0).
    function _pv(bytes memory tx_) internal pure returns (bytes memory) {
        return bytes.concat(
            hex"01", SET_0, bytes2(SHRED_VERSION), bytes8(uint64(50_000)), keccak256("block"),
            hex"00", bytes8(uint64(1000)), bytes8(uint64(1000)), // fast path, all stake
            bytes4(0), bytes4(0), bytes4(0), bytes2(uint16(tx_.length)), tx_
        );
    }
}
