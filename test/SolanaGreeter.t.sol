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
        _execute(_memoTx(CONTROLLER, 1, _memo(block.chainid, "greeter", "hello from solana")));
        assertEq(greeter.greeting(), "hello from solana");
    }

    function test_strangerCannot() public {
        bytes memory pv = _pv(_memoTx(STRANGER, 1, _memo(block.chainid, "greeter", "pwned")));
        bytes memory proof = verifier.proofFor(VKEY, pv);
        vm.expectRevert(SolanaControlled.NotSignedByController.selector);
        greeter.execute(pv, proof);
    }

    function test_noReplay() public {
        bytes memory tx_ = _memoTx(CONTROLLER, 1, _memo(block.chainid, "greeter", "once"));
        _execute(tx_);
        bytes memory pv = _pv(tx_);
        bytes memory proof = verifier.proofFor(VKEY, pv);
        vm.expectRevert(SolanaControlled.AlreadyExecuted.selector);
        greeter.execute(pv, proof);
        // A new transaction (new signature) with the same text is a new command.
        _execute(_memoTx(CONTROLLER, 2, _memo(block.chainid, "greeter", "once")));
    }

    function test_memoForAnotherChainIsIgnored() public {
        bytes memory pv = _pv(_memoTx(CONTROLLER, 1, _memo(8453, "greeter", "base mainnet only")));
        bytes memory proof = verifier.proofFor(VKEY, pv);
        vm.expectRevert(SolanaControlled.NoCommand.selector);
        greeter.execute(pv, proof);
    }

    function test_memoForAnotherContractIsIgnored() public {
        bytes memory pv = _pv(_memoTx(CONTROLLER, 1, _memo(block.chainid, "vault", "withdraw")));
        bytes memory proof = verifier.proofFor(VKEY, pv);
        vm.expectRevert(SolanaControlled.NoCommand.selector);
        greeter.execute(pv, proof);
    }

    function test_forgedCommandRejected() public {
        bytes memory pv = _pv(_memoTx(CONTROLLER, 1, _memo(block.chainid, "greeter", "hello")));
        bytes memory proof = verifier.proofFor(VKEY, pv);
        pv[pv.length - 1] = "X"; // "hellX": not what the controller signed
        vm.expectRevert(BindingVerifier.InvalidProof.selector);
        greeter.execute(pv, proof);
    }

    function test_longCommand() public {
        // 200 bytes: the data length takes two compact-u16 bytes.
        bytes memory long = new bytes(200);
        for (uint256 i; i < long.length; i++) {
            long[i] = "a";
        }
        _execute(_memoTx(CONTROLLER, 1, _memo(block.chainid, "greeter", string(long))));
        assertEq(bytes(greeter.greeting()).length, 200);
    }

    // ---- building blocks ----------------------------------------------------------------

    function _execute(bytes memory tx_) internal {
        bytes memory pv = _pv(tx_);
        greeter.execute(pv, verifier.proofFor(VKEY, pv));
    }

    function _memo(uint256 chain, string memory target, string memory command) internal pure returns (bytes memory) {
        return abi.encodePacked("huginn:", vm.toString(chain), ":", target, ":", command);
    }

    /// Legacy transaction: 1 signature, keys [signer, Memo program], one Memo instruction.
    function _memoTx(bytes32 signer, uint8 nonce, bytes memory memo) internal pure returns (bytes memory) {
        bytes memory sig = abi.encodePacked(bytes32(uint256(nonce)), bytes32(0));
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
