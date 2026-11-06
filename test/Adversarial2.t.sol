// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Test} from "forge-std/Test.sol";
import {SolanaLightClient} from "../src/SolanaLightClient.sol";
import {SolanaControlled} from "../src/SolanaControlled.sol";
import {SolanaGreeter} from "../src/examples/SolanaGreeter.sol";
import {BindingVerifier} from "../src/mocks/BindingVerifier.sol";

/// A command that reverts for the word "boom", like any app whose command can fail on state.
contract Picky is SolanaControlled {
    string public last;

    constructor(SolanaLightClient c, bytes32 controller_) SolanaControlled(c, controller_) {}

    function _execute(bytes memory command) internal override {
        require(keccak256(command) != keccak256("boom"), "command refused");
        last = string(command);
    }
}

/// Round two of the outside review: attacks on the FIXES (nonce queue, cancel, set delay,
/// conflict check). `test_FIXED_*` was a working attack that is now closed. `test_HELD_*` is
/// an attack that was always supposed to fail. `test_LIMIT_*` is a limit that can't be
/// removed in the contract and is documented instead. Run: forge test --match-contract Adversarial2.
/// A command that burns a lot of gas, to test the gas-starvation guard.
contract Hungry is SolanaControlled {
    uint256 public done;

    constructor(SolanaLightClient c, bytes32 controller_) SolanaControlled(c, controller_) {}

    function _execute(bytes memory) internal override {
        uint256 x;
        for (uint256 i; i < 2000; i++) {
            x = uint256(keccak256(abi.encode(x, i)));
            done = x | 2; // never 1 until the end
        }
        done = 1;
    }
}

contract Adversarial2Test is Test {
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
        client = new SolanaLightClient(verifier, VKEY, SHRED_VERSION, 432000, 0, SET_0, 0);
        greeter = new SolanaGreeter(client, CONTROLLER);
    }

    // ---- the cancel path -------------------------------------------------------------------

    /// The controller signs a command, changes their mind, and signs a cancel with the same
    /// nonce. Both are public once finalized, and whichever is relayed first wins. The
    /// attacker already holds the original's proof, the cancel's proof takes minutes to make.
    /// This cannot be fixed in the contract (documented in SolanaControlled): the test pins
    /// the behaviour so nobody mistakes the cancel for a guarantee.
    function test_LIMIT_cancelIsARaceNotAGuarantee() public {
        bytes memory dangerous = _memoTx(CONTROLLER, 1, _memo(greeter, 1, "dangerous"));
        bytes memory cancel = _memoTx(CONTROLLER, 2, _memo(greeter, 1, ""));
        _execute(greeter, dangerous, 50_000); // relayed by whoever saw it first
        _expectRevert(
            greeter, cancel, 50_001, abi.encodeWithSelector(SolanaControlled.WrongNonce.selector, uint64(1), uint64(2))
        );
        assertEq(greeter.greeting(), "dangerous", "the cancel arrived too late");
    }

    /// A command that reverts no longer blocks the queue: the failure is logged, the nonce
    /// is spent, and the next command runs.
    function test_FIXED_aRevertingCommandDoesNotStallTheQueue() public {
        Picky p = new Picky(client, CONTROLLER);
        bytes memory boom = _memoTx(CONTROLLER, 1, _memo2(p, 1, "boom"));
        bytes memory next = _memoTx(CONTROLLER, 2, _memo2(p, 2, "fine"));

        bytes memory pv = _pv(boom, 50_000, SET_0, keccak256("b1"));
        bytes memory boomProof = verifier.proofFor(VKEY, pv);
        vm.expectEmit(true, false, false, false);
        emit SolanaControlled.CommandFailed(1, "");
        p.execute(pv, boomProof);
        assertEq(p.nonce(), 1, "the failed command still spent its nonce");

        pv = _pv(next, 50_001, SET_0, keccak256("b2"));
        p.execute(pv, verifier.proofFor(VKEY, pv));
        assertEq(p.last(), "fine");
    }

    /// A relayer must not be able to make a good command fail by starving it of gas: the
    /// whole call reverts, the nonce stays unspent, and the same proof works with more gas.
    function test_FIXED_aRelayerCannotStarveACommandOfGas() public {
        Hungry h = new Hungry(client, CONTROLLER);
        bytes memory t = _memoTx(CONTROLLER, 1, _memo2(h, 1, "work"));
        bytes memory pv = _pv(t, 50_000, SET_0, keccak256("b1"));
        bytes memory proof = verifier.proofFor(VKEY, pv);

        // Try many gas limits. A call may revert (starved) or finish the command, but it must
        // never succeed with the command half-done and its nonce spent.
        uint256 finished;
        for (uint256 gas = 300_000; gas <= 1_500_000; gas += 5_000) {
            uint256 snap = vm.snapshotState();
            (bool ok,) = address(h).call{gas: gas}(abi.encodeCall(h.execute, (pv, proof)));
            if (ok) {
                assertEq(h.done(), 1, "succeeded without running the command");
                assertEq(h.nonce(), 1);
                finished++;
            } else {
                assertEq(h.nonce(), 0, "a starved call must not spend the nonce");
            }
            vm.revertToState(snap);
        }
        assertGt(finished, 0, "the sweep never reached a gas level that finishes");
        h.execute(pv, proof);
        assertEq(h.done(), 1);
    }

    /// Only `execute` may run a command.
    function test_HELD_runIsNotCallableFromOutside() public {
        vm.expectRevert(SolanaControlled.OnlySelf.selector);
        greeter.run("hi");
    }

    // ---- the conflicting-block check ---------------------------------------------------------

    /// A block recorded while an attacker's set was in force no longer blocks the real proof
    /// for that slot once the real set is back: records are kept per validator set.
    function test_FIXED_aBadSetEpisodeLeavesNoConflictPoison() public {
        SolanaLightClient slow = new SolanaLightClient(verifier, VKEY, SHRED_VERSION, 432000, 0, SET_0, 1 days);
        SolanaGreeter g = new SolanaGreeter(slow, CONTROLLER);
        bytes32 evilSet = keccak256("attacker set");

        slow.proposeValidatorSet(0, evilSet); // compromised owner key
        vm.warp(block.timestamp + 1 days);
        bytes memory fake = _memoTx(CONTROLLER, 1, _memo(g, 1, "irrelevant"));
        bytes memory fakePv = _pv(fake, 50_000, evilSet, keccak256("fake block"));
        slow.submit(fakePv, verifier.proofFor(VKEY, fakePv)); // records slot 50_000 under evilSet

        slow.proposeValidatorSet(0, SET_0); // owner recovers
        vm.warp(block.timestamp + 1 days);

        bytes memory real = _memoTx(CONTROLLER, 2, _memo(g, 1, "a real, finalized command"));
        bytes memory realPv = _pv(real, 50_000, SET_0, keccak256("the real block"));
        g.execute(realPv, verifier.proofFor(VKEY, realPv)); // not refused
        assertEq(g.greeting(), "a real, finalized command");
    }

    /// Two different blocks for one slot under the SAME set is still refused.
    function test_HELD_conflictUnderOneSetIsStillRefused() public {
        bytes memory a = _memoTx(CONTROLLER, 1, _memo(greeter, 1, "a"));
        bytes memory pvA = _pv(a, 50_000, SET_0, keccak256("block A"));
        client.submit(pvA, verifier.proofFor(VKEY, pvA));
        bytes memory b = _memoTx(CONTROLLER, 2, _memo(greeter, 1, "b"));
        bytes memory pvB = _pv(b, 50_000, SET_0, keccak256("block B"));
        bytes memory proofB = verifier.proofFor(VKEY, pvB);
        vm.expectRevert(
            abi.encodeWithSelector(
                SolanaLightClient.ConflictingBlock.selector, uint64(50_000), keccak256("block A"), keccak256("block B")
            )
        );
        client.submit(pvB, proofB);
    }

    // ---- the set delay -----------------------------------------------------------------------

    /// The delay cannot be skipped: not by proposing again, not by settling early, not by
    /// proposing an epoch that already has a set.
    function test_HELD_theDelayCannotBeShortened() public {
        SolanaLightClient slow = new SolanaLightClient(verifier, VKEY, SHRED_VERSION, 432000, 0, SET_0, 1 days);
        SolanaGreeter g = new SolanaGreeter(slow, CONTROLLER);
        bytes32 evil = keccak256("evil");
        slow.proposeValidatorSet(0, evil); // epoch 0 already has SET_0
        bytes memory t = _memoTx(CONTROLLER, 1, _memo(g, 1, "x"));
        bytes memory pv = _pv(t, 50_000, evil, keccak256("b"));
        bytes memory proof = verifier.proofFor(VKEY, pv);

        vm.expectRevert(abi.encodeWithSelector(SolanaLightClient.WrongValidatorSet.selector, evil, SET_0));
        g.execute(pv, proof); // the old set stays in force during the wait

        vm.warp(block.timestamp + 1 days - 1);
        slow.settleValidatorSet(0); // too early: must not promote it
        assertEq(slow.validatorSetHash(0), SET_0);
        slow.proposeValidatorSet(0, evil); // proposing again restarts the wait
        vm.warp(block.timestamp + 1 days - 1);
        assertEq(slow.validatorSetHash(0), SET_0, "restarted wait must not have matured");
        vm.warp(block.timestamp + 1);
        assertEq(slow.validatorSetHash(0), evil);
    }

    /// Someone who is not the owner can do nothing with proposals or ownership.
    function test_HELD_strangersCannotTouchSetsOrOwnership() public {
        address stranger = address(0xbad);
        vm.startPrank(stranger);
        vm.expectRevert(SolanaLightClient.NotOwner.selector);
        client.proposeValidatorSet(7, keccak256("x"));
        vm.expectRevert(SolanaLightClient.NotOwner.selector);
        client.cancelValidatorSet(7);
        vm.expectRevert(SolanaLightClient.NotOwner.selector);
        client.transferOwnership(stranger);
        vm.expectRevert(SolanaLightClient.NotPendingOwner.selector);
        client.acceptOwnership();
        vm.stopPrank();
        // the zero address can't "accept" an unset pending owner either
        vm.prank(address(0));
        vm.expectRevert(SolanaLightClient.NotPendingOwner.selector);
        client.acceptOwnership();
    }

    // ---- nonce spellings ---------------------------------------------------------------------

    /// Every odd way of writing the nonce is refused, so one nonce has exactly one spelling.
    function test_HELD_oddNonceSpellingsAreRefused() public {
        string[9] memory bad =
            ["+1:x", "01:x", "1 :x", " 1:x", "1;x", "1", ":x", "99999999999999999999:x", unicode"١:x"];
        for (uint256 i; i < bad.length; i++) {
            bytes memory memo = abi.encodePacked(greeter.memoPrefix(), bad[i]);
            bytes memory pv = _pv(_memoTx(CONTROLLER, uint8(i + 1), memo), 50_000, SET_0, keccak256("b"));
            bytes memory proof = verifier.proofFor(VKEY, pv);
            (bool ok,) = address(greeter).call(abi.encodeCall(greeter.execute, (pv, proof)));
            assertFalse(ok, bad[i]);
        }
        assertEq(greeter.nonce(), 0);
    }

    /// The address may be written in any letter case, but that is still one nonce: the second
    /// spelling of the same command cannot run again.
    function test_HELD_addressCaseDoesNotGiveASecondNonce() public {
        string memory lower = _lower(vm.toString(address(greeter)));
        bytes memory a = abi.encodePacked("huginn:", vm.toString(block.chainid), ":", lower, ":1:one");
        bytes memory b =
            abi.encodePacked("huginn:", vm.toString(block.chainid), ":", vm.toString(address(greeter)), ":1:one");
        _execute(greeter, _memoTx(CONTROLLER, 1, a), 50_000);
        _expectRevert(
            greeter,
            _memoTx(CONTROLLER, 2, b),
            50_001,
            abi.encodeWithSelector(SolanaControlled.WrongNonce.selector, uint64(1), uint64(2))
        );
    }

    // ---- building blocks -----------------------------------------------------------------------

    function _execute(SolanaGreeter g, bytes memory tx_, uint64 slot) internal {
        bytes memory pv = _pv(tx_, slot, SET_0, keccak256(abi.encodePacked("block", slot)));
        g.execute(pv, verifier.proofFor(VKEY, pv));
    }

    function _expectRevert(SolanaGreeter g, bytes memory tx_, uint64 slot, bytes memory err) internal {
        bytes memory pv = _pv(tx_, slot, SET_0, keccak256(abi.encodePacked("block", slot)));
        bytes memory proof = verifier.proofFor(VKEY, pv);
        vm.expectRevert(err);
        g.execute(pv, proof);
    }

    function _memo(SolanaGreeter g, uint256 nonce, string memory command) internal view returns (bytes memory) {
        return abi.encodePacked(g.memoPrefix(), vm.toString(nonce), ":", command);
    }

    function _memo2(SolanaControlled p, uint256 nonce, string memory command) internal view returns (bytes memory) {
        return abi.encodePacked(p.memoPrefix(), vm.toString(nonce), ":", command);
    }

    function _lower(string memory s) internal pure returns (string memory) {
        bytes memory b = bytes(s);
        for (uint256 i; i < b.length; i++) {
            if (b[i] >= "A" && b[i] <= "Z") b[i] = bytes1(uint8(b[i]) + 32);
        }
        return string(b);
    }

    function _memoTx(bytes32 signer, uint8 sigSeed, bytes memory memo) internal pure returns (bytes memory) {
        bytes memory sig = abi.encodePacked(bytes32(uint256(sigSeed)), bytes32(0));
        return bytes.concat(
            hex"01",
            sig,
            hex"010001",
            hex"02",
            signer,
            MEMO,
            keccak256("blockhash"),
            hex"01",
            hex"01",
            hex"0100",
            _compactU16(memo.length),
            memo
        );
    }

    function _compactU16(uint256 n) internal pure returns (bytes memory) {
        if (n < 0x80) return abi.encodePacked(uint8(n));
        if (n < 0x4000) return abi.encodePacked(uint8(n & 0x7f) | 0x80, uint8(n >> 7));
        return abi.encodePacked(uint8(n & 0x7f) | 0x80, uint8((n >> 7) & 0x7f) | 0x80, uint8(n >> 14));
    }

    function _pv(bytes memory tx_, uint64 slot, bytes32 setHash, bytes32 blockId) internal pure returns (bytes memory) {
        return bytes.concat(
            hex"01",
            setHash,
            bytes2(SHRED_VERSION),
            bytes8(slot),
            blockId,
            hex"00",
            bytes8(uint64(1000)),
            bytes8(uint64(1000)),
            bytes4(0),
            bytes4(0),
            bytes4(0),
            bytes2(uint16(tx_.length)),
            tx_
        );
    }
}
