// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Test} from "forge-std/Test.sol";
import {SolanaLightClient} from "../src/SolanaLightClient.sol";
import {SolanaControlled} from "../src/SolanaControlled.sol";
import {SolanaGreeter} from "../src/examples/SolanaGreeter.sol";
import {BindingVerifier} from "../src/mocks/BindingVerifier.sol";

/// Adversarial proofs-of-concept from an outside review. NOT YET RUN: the reviewer's machine
/// had no Foundry, so run `forge test --match-contract Adversarial -vv` and treat any compile
/// error as a bug in this file, not a finding.
///
/// Every test PASSES when the weakness is present (it asserts the bad outcome). If you fix a
/// weakness, flip the matching test to expect the revert.
///
/// The stand-in verifier lets the test mint a "proof" for any public values. That is the
/// same power a real prover has over values it can legitimately prove, so each test only
/// builds public values a real prover could obtain (a real Solana transaction in a real
/// finalized block), except `ownerKeyForges...`, which uses the owner's trust power.
contract AdversarialTest is Test {
    uint16 constant SHRED_VERSION = 28090;
    bytes32 constant SET_0 = 0xb1f25236519c10b064498b6507ed0b87b88ad4df728a0a348e9434769d7d3c11;
    bytes32 constant MEMO = 0x054a53522854cdd103cc2e4631a3f51792ac727cf1e50abc97828ac5994f3cc2;
    bytes32 constant VKEY = bytes32(uint256(0x000482e7));
    bytes32 constant CONTROLLER = keccak256("the controller's Solana key");

    BindingVerifier verifier;
    SolanaLightClient client;
    SolanaGreeter greeter;

    function setUp() public {
        verifier = new BindingVerifier();
        client = new SolanaLightClient(verifier, VKEY, SHRED_VERSION, 432000, 0, SET_0);
        greeter = new SolanaGreeter(client, CONTROLLER);
    }

    // ---- F1: no ordering, no expiry, no deployment binding ---------------------------------

    /// The controller signs "set A" and then "set B", meaning the greeting should end as B.
    /// Anyone can relay the proofs in any order, so the attacker makes the stale command win.
    function test_POC_commandsCanBeRelayedOutOfOrder() public {
        bytes memory first = _memoTx(CONTROLLER, 1, _memo("greeter", "A (old, superseded)"));
        bytes memory second = _memoTx(CONTROLLER, 2, _memo("greeter", "B (latest intent)"));
        _execute(greeter, second, 50_001);
        _execute(greeter, first, 50_000); // relayed late, by anyone
        assertEq(greeter.greeting(), "A (old, superseded)", "the older command overwrote the newer one");
    }

    /// A command the controller signed for one deployment runs on any other deployment that
    /// uses the same target name and controller (a redeploy, a v2, a fork of the contract).
    function test_POC_oldCommandReplaysOnANewDeployment() public {
        bytes memory old = _memoTx(CONTROLLER, 1, _memo("greeter", "sign me once"));
        _execute(greeter, old, 50_000);

        SolanaGreeter v2 = new SolanaGreeter(client, CONTROLLER);
        assertEq(v2.greeting(), "gm");
        _execute(v2, old, 50_000); // same transaction, never meant for v2
        assertEq(v2.greeting(), "sign me once");
    }

    /// There is no freshness check: a command that was never relayed stays valid forever.
    function test_POC_unrelayedCommandNeverExpires() public {
        bytes memory old = _memoTx(CONTROLLER, 1, _memo("greeter", "dormant"));
        vm.warp(block.timestamp + 5 * 365 days);
        _execute(greeter, old, 50_000);
        assertEq(greeter.greeting(), "dormant");
    }

    /// The replay guard is the transaction's first signature. Two signatures over the SAME
    /// message (what a malleable signature would give) are two different "transactions".
    /// Safe only while Solana never produces a second valid signature for a message.
    function test_POC_replayGuardIsKeyedOnSignatureNotMessage() public {
        bytes memory m = _memo("greeter", "same message");
        _execute(greeter, _memoTx(CONTROLLER, 1, m), 50_000);
        _execute(greeter, _memoTx(CONTROLLER, 2, m), 50_000); // same message, other signature bytes
        assertTrue(greeter.executed(keccak256(abi.encodePacked(bytes32(uint256(1)), bytes32(0)))));
        assertTrue(greeter.executed(keccak256(abi.encodePacked(bytes32(uint256(2)), bytes32(0)))));
    }

    // ---- F2: the validator-set owner is the root of trust ----------------------------------

    /// The owner stores the set hash for each epoch. Owner can store the hash of a set whose
    /// keys it controls, then prove any transaction, signed by anyone, finalized by that
    /// fake "validator set". Every consumer, including SolanaControlled, accepts it.
    function test_POC_ownerKeyForgesAnyControllerCommand() public {
        bytes32 evilSet = keccak256("attacker's own validator set");
        client.storeValidatorSet(1, evilSet); // this test contract is the owner
        bytes memory forged = _memoTx(CONTROLLER, 9, _memo("greeter", "owned by whoever holds the owner key"));
        bytes memory pv = _pv(forged, 432_001, evilSet, keccak256("a block that never existed"));
        greeter.execute(pv, verifier.proofFor(VKEY, pv));
        assertEq(greeter.greeting(), "owned by whoever holds the owner key");
    }

    /// Write-once means a typo is permanent: that epoch can never be proven, and the
    /// client's verifier, program key and every consumer's pointer to it are immutable, so
    /// recovery is a redeploy of the client and all apps.
    function test_POC_wrongSetForAnEpochBricksItForever() public {
        bytes32 typo = keccak256("typo");
        bytes32 realSet1 = keccak256("the correct epoch 1 set");
        client.storeValidatorSet(1, typo);
        vm.expectRevert(abi.encodeWithSelector(SolanaLightClient.SetAlreadyStored.selector, uint64(1)));
        client.storeValidatorSet(1, realSet1);

        bytes memory tx_ = _memoTx(CONTROLLER, 1, _memo("greeter", "x"));
        bytes memory pv = _pv(tx_, 432_001, realSet1, keccak256("real block"));
        bytes memory proof = verifier.proofFor(VKEY, pv);
        vm.expectRevert(abi.encodeWithSelector(SolanaLightClient.WrongValidatorSet.selector, realSet1, typo));
        client.verify(pv, proof);
    }

    /// A single owner transaction can also hand the client to a dead address by mistake
    /// (no two-step transfer): the next epoch's set can then never be stored.
    function test_POC_setOwnerHasNoTwoStepConfirmation() public {
        address typoAddress = address(0xdead);
        client.setOwner(typoAddress);
        vm.expectRevert(SolanaLightClient.NotOwner.selector);
        client.storeValidatorSet(1, keccak256("next epoch"));
    }

    // ---- F5: `verify` ignores the conflicting-block record ---------------------------------

    /// `submit` refuses a second block at an already-finalized slot (ConflictingBlock), but
    /// `verify`, which SolanaControlled uses, never looks at that record.
    function test_POC_applicationsBypassTheConflictingBlockCheck() public {
        bytes memory t1 = _memoTx(CONTROLLER, 1, _memo("greeter", "on block A"));
        bytes memory pv1 = _pv(t1, 50_000, SET_0, keccak256("block A"));
        client.submit(pv1, verifier.proofFor(VKEY, pv1));

        bytes memory t2 = _memoTx(CONTROLLER, 2, _memo("greeter", "on block B"));
        bytes memory pv2 = _pv(t2, 50_000, SET_0, keccak256("block B"));
        bytes memory proof2 = verifier.proofFor(VKEY, pv2);
        vm.expectRevert(SolanaLightClient.ConflictingBlock.selector);
        client.submit(pv2, proof2);

        greeter.execute(pv2, proof2); // the application path never consults finalizedBlock
        assertEq(greeter.greeting(), "on block B");
    }

    // ---- building blocks (same layout as SolanaGreeter.t.sol) ------------------------------

    function _execute(SolanaGreeter g, bytes memory tx_, uint64 slot) internal {
        bytes memory pv = _pv(tx_, slot, SET_0, keccak256(abi.encodePacked("block", slot)));
        g.execute(pv, verifier.proofFor(VKEY, pv));
    }

    function _memo(string memory target, string memory command) internal view returns (bytes memory) {
        return abi.encodePacked("huginn:", vm.toString(block.chainid), ":", target, ":", command);
    }

    /// Legacy transaction: 1 signature, keys [signer, Memo program], one Memo instruction.
    /// `nonce` only changes the signature bytes, never the message.
    function _memoTx(bytes32 signer, uint8 nonce, bytes memory memo) internal pure returns (bytes memory) {
        bytes memory sig = abi.encodePacked(bytes32(uint256(nonce)), bytes32(0));
        return bytes.concat(
            hex"01", sig,
            hex"010001",
            hex"02", signer, MEMO,
            keccak256("blockhash"),
            hex"01", hex"01", hex"0100",
            _compactU16(memo.length), memo
        );
    }

    function _compactU16(uint256 n) internal pure returns (bytes memory) {
        if (n < 0x80) return abi.encodePacked(uint8(n));
        if (n < 0x4000) return abi.encodePacked(uint8(n & 0x7f) | 0x80, uint8(n >> 7));
        return abi.encodePacked(uint8(n & 0x7f) | 0x80, uint8((n >> 7) & 0x7f) | 0x80, uint8(n >> 14));
    }

    function _pv(bytes memory tx_, uint64 slot, bytes32 setHash, bytes32 blockId) internal pure returns (bytes memory) {
        return bytes.concat(
            hex"01", setHash, bytes2(SHRED_VERSION), bytes8(slot), blockId,
            hex"00", bytes8(uint64(1000)), bytes8(uint64(1000)),
            bytes4(0), bytes4(0), bytes4(0), bytes2(uint16(tx_.length)), tx_
        );
    }
}
