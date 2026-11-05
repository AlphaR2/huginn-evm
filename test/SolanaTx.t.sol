// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Test} from "forge-std/Test.sol";
import {SolanaTx} from "../src/SolanaTx.sol";

contract TxHarness {
    function parse(bytes memory raw) external pure returns (SolanaTx.Transaction memory) {
        return SolanaTx.parse(raw);
    }

    function firstSignature(bytes memory raw) external pure returns (bytes memory) {
        return SolanaTx.firstSignature(raw);
    }
}

contract SolanaTxTest is Test {
    using SolanaTx for SolanaTx.Transaction;

    bytes32 constant PAYER = 0xa120a7e1071a6c25ef5343be2497144cb7fca9e4b8a28c0ee266709a6c6a8516;
    bytes32 constant RECIPIENT = 0x62b02bc726ffe93717028aeeeb9ef6963be8f3f5f3e7aedfe15852800d92d788;
    bytes32 constant SYSTEM_PROGRAM = bytes32(0);
    // MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr
    bytes32 constant MEMO_PROGRAM = 0x054a53522854cdd103cc2e4631a3f51792ac727cf1e50abc97828ac5994f3cc2;

    TxHarness h;
    bytes raw;
    bytes sig;

    function setUp() public {
        string memory j = vm.readFile("test/fixtures/tx_46718.json");
        raw = vm.parseBytes(string.concat("0x", vm.parseJsonString(j, ".raw")));
        sig = vm.parseBytes(string.concat("0x", vm.parseJsonString(j, ".signature")));
        h = new TxHarness();
    }

    /// The real memo transfer from the local chain: 0.5 SOL plus a memo.
    function test_realMemoTransfer() public view {
        SolanaTx.Transaction memory t = h.parse(raw);
        assertFalse(t.versioned);
        assertEq(t.signatures.length, 1);
        assertEq(t.signatures[0], sig);
        assertEq(SolanaTx.firstSignature(raw), sig);
        assertEq(t.numRequiredSignatures, 1);
        assertEq(t.accountKeys.length, 4);
        assertEq(t.recentBlockhash, 0xc60f08a9df62aac45aebfc9c3433d5b13197086ccc50898e5fd78c89d2daf4ae);

        assertEq(t.signer(0), PAYER);
        assertTrue(t.isSigner(PAYER));
        assertFalse(t.isSigner(RECIPIENT));

        assertEq(t.instructions.length, 2);
        assertEq(t.programId(0), SYSTEM_PROGRAM);
        assertEq(t.account(0, 0), PAYER);
        assertEq(t.account(0, 1), RECIPIENT);
        // System transfer: tag 2 (u32 LE), lamports 500_000_000 (u64 LE).
        assertEq(t.instructions[0].data, hex"020000000065cd1d00000000");

        assertEq(t.programId(1), MEMO_PROGRAM);
        assertEq(string(t.instructions[1].data), "huginn: hello from solana");
    }

    function test_signerOutOfRange() public {
        SolanaTx.Transaction memory t = h.parse(raw);
        vm.expectRevert(SolanaTx.IndexOutOfRange.selector);
        this.signerOf(t, 1);
    }

    function signerOf(SolanaTx.Transaction memory t, uint256 i) external pure returns (bytes32) {
        return t.signer(i);
    }

    function test_rejectsTrailingByte() public {
        vm.expectRevert(abi.encodeWithSelector(SolanaTx.Malformed.selector, raw.length));
        h.parse(bytes.concat(raw, hex"00"));
    }

    function test_rejectsEveryTruncation() public {
        for (uint256 len; len < raw.length; len++) {
            bytes memory cut = new bytes(len);
            for (uint256 i; i < len; i++) {
                cut[i] = raw[i];
            }
            vm.expectRevert();
            h.parse(cut);
        }
    }

    function test_rejectsNonCanonicalLength() public {
        // Signature count 1 written as two bytes (0x81 0x00) instead of one. A first byte
        // of 0x81 means v1, so this is read as (and refused as) a broken v1 transaction.
        bytes memory b = bytes.concat(hex"8100", _slice(raw, 1, raw.length));
        vm.expectRevert();
        h.parse(b);
    }

    function test_rejectsUnknownFirstByte() public {
        // 0x82 would be a "v2" transaction: no such format.
        bytes memory b = bytes.concat(hex"82", _slice(raw, 1, raw.length));
        vm.expectRevert(abi.encodeWithSelector(SolanaTx.UnsupportedVersion.selector, 2));
        h.parse(b);
    }

    /// A v1 transaction serialized by Agave's SDK (huginn's fixtures/tx_v1_agave.json).
    function test_parsesV1FromAgave() public view {
        bytes memory v1 = _v1();
        SolanaTx.Transaction memory t = h.parse(v1);
        assertTrue(t.versioned);
        assertEq(t.version, 1);
        assertEq(t.signatures.length, 2);
        assertEq(t.signatures[0], _repeat(0x11, 64));
        assertEq(t.signatures[1], _repeat(0x22, 64));
        assertEq(t.numRequiredSignatures, 2);
        assertEq(t.numReadonlySigned, 1);
        assertEq(t.numReadonlyUnsigned, 1);
        assertEq(t.accountKeys.length, 4);
        assertEq(t.accountKeys[0], bytes32(_repeat(0xa1, 32)));
        assertEq(t.accountKeys[3], MEMO_PROGRAM);
        assertEq(t.recentBlockhash, bytes32(_repeat(0x77, 32)));
        assertEq(t.instructions.length, 2);
        assertEq(t.programId(0), MEMO_PROGRAM);
        assertEq(t.instructions[0].accounts, hex"0001");
        assertEq(string(t.instructions[0].data), "huginn: hello from a v1 transaction");
        assertEq(t.instructions[1].data, _repeat(0x61, 300));
        assertEq(t.lookups.length, 0);
        assertTrue(t.isSigner(bytes32(_repeat(0xb2, 32))));
        assertFalse(t.isSigner(bytes32(_repeat(0xc3, 32))));
        // The id is the first signature, which a v1 transaction keeps at the end.
        assertEq(h.firstSignature(v1), _repeat(0x11, 64));
    }

    function test_v1RejectsEveryTruncationAndTrailingByte() public {
        bytes memory v1 = _v1();
        for (uint256 n; n < v1.length; n++) {
            try h.parse(_slice(v1, 0, n)) {
                revert("a truncated v1 transaction parsed");
            } catch {}
        }
        vm.expectRevert();
        h.parse(bytes.concat(v1, hex"00"));
    }

    function test_v1RejectsBadConfigMask() public {
        bytes memory v1 = _v1();
        v1[4] = 0x20; // an unknown config bit
        vm.expectRevert(abi.encodeWithSelector(SolanaTx.Malformed.selector, 4));
        h.parse(v1);
        v1[4] = 0x01; // half a priority fee
        vm.expectRevert(abi.encodeWithSelector(SolanaTx.Malformed.selector, 4));
        h.parse(v1);
    }

    function _v1() internal view returns (bytes memory) {
        string memory j = vm.readFile("test/fixtures/tx_v1_agave.json");
        return vm.parseBytes(string.concat("0x", vm.parseJsonString(j, ".raw")));
    }

    function _repeat(uint8 b, uint256 n) internal pure returns (bytes memory out) {
        out = new bytes(n);
        for (uint256 i; i < n; i++) {
            out[i] = bytes1(b);
        }
    }

    function test_rejectsUnknownMessageVersion() public {
        // Insert a version byte for v1 right after the signature.
        bytes memory b = bytes.concat(_slice(raw, 0, 65), hex"81", _slice(raw, 65, raw.length));
        vm.expectRevert(abi.encodeWithSelector(SolanaTx.UnsupportedVersion.selector, 1));
        h.parse(b);
    }

    function test_parsesV0WithLookup() public view {
        // The same message as v0 with one lookup table (1 writable, 2 readonly indexes).
        bytes32 table = keccak256("table");
        bytes memory b = bytes.concat(
            _slice(raw, 0, 65), hex"80", _slice(raw, 65, raw.length), hex"01", table, hex"0105", hex"020607"
        );
        SolanaTx.Transaction memory t = h.parse(b);
        assertTrue(t.versioned);
        assertEq(t.lookups.length, 1);
        assertEq(t.lookups[0].table, table);
        assertEq(t.lookups[0].writable, hex"05");
        assertEq(t.lookups[0].readonly, hex"0607");
        assertEq(string(t.instructions[1].data), "huginn: hello from solana");
    }

    /// Random bytes never panic: either a clean parse or a revert with our error.
    function testFuzz_noPanic(bytes memory junk) public {
        try h.parse(junk) {}
        catch (bytes memory err) {
            bytes4 s = bytes4(err);
            assertTrue(
                s == SolanaTx.Malformed.selector || s == SolanaTx.UnsupportedVersion.selector, "unexpected error"
            );
        }
    }

    function _slice(bytes memory b, uint256 from, uint256 to) internal pure returns (bytes memory out) {
        out = new bytes(to - from);
        for (uint256 i; i < out.length; i++) {
            out[i] = b[from + i];
        }
    }
}
