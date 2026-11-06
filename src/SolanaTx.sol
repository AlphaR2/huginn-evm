// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// Reads a Solana transaction (legacy, v0 or v1, wire format) on the destination chain, so
/// contracts can act on who signed it and what it called. Same layouts as
/// `crates/alpenglow-core/src/tx.rs`.
///
/// Legacy and v0 (first byte below 0x80):
///
///   signatures   compact-u16 n, n * 64 bytes
///   [version]    one byte 0x80 | v, only for versioned messages (v0 is the only one)
///   header       num_required_signatures, num_readonly_signed, num_readonly_unsigned
///   account keys compact-u16 n, n * 32 bytes
///   blockhash    32 bytes
///   instructions compact-u16 n, each: program_id_index u8, compact-u16 + account
///                indexes, compact-u16 + data
///   [lookups]    v0 only: compact-u16 n, each: table key 32, compact-u16 + writable
///                indexes, compact-u16 + readonly indexes
///
/// v1 (first byte 0x81, SIMD-0385, live on mainnet since epoch 1035): message first,
/// signatures last, single-byte counts, no lookup tables:
///
///   0x81, header (3 bytes), config mask u32 LE, blockhash 32,
///   num_instructions u8, num_addresses u8, addresses 32 each,
///   config values (8 bytes if mask bits 0+1, then 4 bytes each for bits 2, 3, 4),
///   instruction headers (program_id_index u8, num_accounts u8, data_len u16 LE) each,
///   then each instruction's accounts and data,
///   signatures: num_required_signatures * 64 bytes
///
/// A transaction that came out of a Huginn proof already passed the full sanity check in
/// SP1 (signature count, fee payer, index ranges, Agave's v1 rules). This parser checks
/// bounds, canonical lengths and that every byte is consumed, so unproven input reverts
/// cleanly instead of misbehaving. But what it reads from unproven input means nothing:
/// anyone can write bytes that claim any signer. Only trust a transaction a proof vouches
/// for (`SolanaLightClient.verify`).
library SolanaTx {
    error Malformed(uint256 offset);
    error UnsupportedVersion(uint8 version);
    error IndexOutOfRange();

    struct Instruction {
        uint8 programIdIndex;
        bytes accounts;
        bytes data;
    }

    struct Lookup {
        bytes32 table;
        bytes writable;
        bytes readonly;
    }

    struct Transaction {
        bytes[] signatures;
        /// False for legacy. True for v0 and v1; `version` says which.
        bool versioned;
        uint8 version;
        uint8 numRequiredSignatures;
        uint8 numReadonlySigned;
        uint8 numReadonlyUnsigned;
        bytes32[] accountKeys;
        bytes32 recentBlockhash;
        Instruction[] instructions;
        Lookup[] lookups;
    }

    uint8 private constant V1_PREFIX = 0x81;
    uint32 private constant V1_KNOWN_BITS = 0x1f;

    function parse(bytes memory raw) internal pure returns (Transaction memory t) {
        uint8 first = _u8(raw, 0);
        if (first == V1_PREFIX) return _parseV1(raw);
        if (first & 0x80 != 0) revert UnsupportedVersion(first & 0x7f);
        uint256 p = 0;
        uint256 n;

        (n, p) = _count(raw, p, 64);
        t.signatures = new bytes[](n);
        for (uint256 i; i < n; i++) {
            (t.signatures[i], p) = _take(raw, p, 64);
        }

        if (_u8(raw, p) & 0x80 != 0) {
            uint8 version = _u8(raw, p) & 0x7f;
            if (version != 0) revert UnsupportedVersion(version);
            t.versioned = true;
            p += 1;
        }
        t.numRequiredSignatures = _u8(raw, p);
        t.numReadonlySigned = _u8(raw, p + 1);
        t.numReadonlyUnsigned = _u8(raw, p + 2);
        p += 3;

        (n, p) = _count(raw, p, 32);
        t.accountKeys = new bytes32[](n);
        for (uint256 i; i < n; i++) {
            (t.accountKeys[i], p) = _bytes32(raw, p);
        }
        (t.recentBlockhash, p) = _bytes32(raw, p);

        (n, p) = _count(raw, p, 3);
        t.instructions = new Instruction[](n);
        for (uint256 i; i < n; i++) {
            Instruction memory ix = t.instructions[i];
            ix.programIdIndex = _u8(raw, p);
            p += 1;
            (ix.accounts, p) = _lengthPrefixed(raw, p);
            (ix.data, p) = _lengthPrefixed(raw, p);
        }

        if (t.versioned) {
            (n, p) = _count(raw, p, 34);
            t.lookups = new Lookup[](n);
            for (uint256 i; i < n; i++) {
                Lookup memory l = t.lookups[i];
                (l.table, p) = _bytes32(raw, p);
                (l.writable, p) = _lengthPrefixed(raw, p);
                (l.readonly, p) = _lengthPrefixed(raw, p);
            }
        }
        if (p != raw.length) revert Malformed(p);
    }

    /// The transaction's id on Solana (what explorers and RPC call its signature): its first
    /// signature, which a v1 transaction keeps at the end.
    function firstSignature(bytes memory raw) internal pure returns (bytes memory sig) {
        if (_u8(raw, 0) == V1_PREFIX) {
            uint256 count = _u8(raw, 1);
            if (count == 0 || raw.length < 4 + count * 64) revert Malformed(1);
            (sig,) = _take(raw, raw.length - count * 64, 64);
            return sig;
        }
        (uint256 n, uint256 p) = _compactU16(raw, 0);
        if (n == 0) revert Malformed(0);
        (sig,) = _take(raw, p, 64);
    }

    /// Signer `i` (0 is the fee payer). Signers are the first account keys.
    function signer(Transaction memory t, uint256 i) internal pure returns (bytes32) {
        if (i >= t.numRequiredSignatures) revert IndexOutOfRange();
        return t.accountKeys[i];
    }

    function isSigner(Transaction memory t, bytes32 key) internal pure returns (bool) {
        for (uint256 i; i < t.numRequiredSignatures; i++) {
            if (t.accountKeys[i] == key) return true;
        }
        return false;
    }

    /// Program ids are always static keys, never loaded from a lookup table.
    function programId(Transaction memory t, uint256 ix) internal pure returns (bytes32) {
        uint8 idx = t.instructions[ix].programIdIndex;
        if (idx >= t.accountKeys.length) revert IndexOutOfRange();
        return t.accountKeys[idx];
    }

    /// Account `j` of instruction `ix`, if it is a static key. Accounts loaded from a v0
    /// lookup table can't be resolved here (that needs the table's contents) and revert.
    function account(Transaction memory t, uint256 ix, uint256 j) internal pure returns (bytes32) {
        uint8 idx = uint8(t.instructions[ix].accounts[j]);
        if (idx >= t.accountKeys.length) revert IndexOutOfRange();
        return t.accountKeys[idx];
    }

    function _parseV1(bytes memory raw) private pure returns (Transaction memory t) {
        t.versioned = true;
        t.version = 1;
        t.numRequiredSignatures = _u8(raw, 1);
        t.numReadonlySigned = _u8(raw, 2);
        t.numReadonlyUnsigned = _u8(raw, 3);
        uint32 mask =
            uint32(_u8(raw, 4)) | uint32(_u8(raw, 5)) << 8 | uint32(_u8(raw, 6)) << 16 | uint32(_u8(raw, 7)) << 24;
        uint32 fee = mask & 3;
        if (mask & ~V1_KNOWN_BITS != 0 || fee == 1 || fee == 2) revert Malformed(4);
        uint256 p;
        (t.recentBlockhash, p) = _bytes32(raw, 8);
        uint256 numIx = _u8(raw, p);
        uint256 numKeys = _u8(raw, p + 1);
        p += 2;
        if (p + numKeys * 32 > raw.length) revert Malformed(p);
        t.accountKeys = new bytes32[](numKeys);
        for (uint256 i; i < numKeys; i++) {
            (t.accountKeys[i], p) = _bytes32(raw, p);
        }
        // Config values (priority fee, compute unit limit, loaded data size, heap size).
        p += (fee == 3 ? 8 : 0) + (mask & 4 != 0 ? 4 : 0) + (mask & 8 != 0 ? 4 : 0) + (mask & 16 != 0 ? 4 : 0);

        uint256 h = p; // instruction headers, 4 bytes each
        p += numIx * 4;
        if (p > raw.length) revert Malformed(h);
        t.instructions = new Instruction[](numIx);
        for (uint256 i; i < numIx; i++) {
            Instruction memory ix = t.instructions[i];
            uint256 at = h + 4 * i;
            ix.programIdIndex = uint8(raw[at]);
            (ix.accounts, p) = _take(raw, p, uint8(raw[at + 1]));
            (ix.data, p) = _take(raw, p, uint256(uint8(raw[at + 2])) | uint256(uint8(raw[at + 3])) << 8);
        }

        uint256 numSigs = t.numRequiredSignatures;
        if (p + numSigs * 64 != raw.length) revert Malformed(p);
        t.signatures = new bytes[](numSigs);
        for (uint256 i; i < numSigs; i++) {
            (t.signatures[i], p) = _take(raw, p, 64);
        }
    }

    function _u8(bytes memory b, uint256 p) private pure returns (uint8) {
        if (p >= b.length) revert Malformed(p);
        return uint8(b[p]);
    }

    /// Minimal encodings only, up to u16::MAX, same as `Reader::compact_u16`.
    function _compactU16(bytes memory b, uint256 p) private pure returns (uint256 value, uint256 next) {
        for (uint256 i; i < 3; i++) {
            uint8 byte_ = _u8(b, p + i);
            value |= uint256(byte_ & 0x7f) << (7 * i);
            if (byte_ & 0x80 == 0) {
                if ((i > 0 && byte_ == 0) || value > type(uint16).max) revert Malformed(p);
                return (value, p + i + 1);
            }
        }
        revert Malformed(p);
    }

    /// A count of items at least `minSize` bytes each, checked against the bytes left before
    /// anything is allocated, so a forged huge count reverts instead of burning gas.
    function _count(bytes memory b, uint256 p, uint256 minSize) private pure returns (uint256 n, uint256 next) {
        (n, next) = _compactU16(b, p);
        if (next + n * minSize > b.length) revert Malformed(p);
    }

    function _take(bytes memory b, uint256 p, uint256 len) private pure returns (bytes memory out, uint256 next) {
        if (p + len > b.length) revert Malformed(p);
        out = new bytes(len);
        for (uint256 i; i < len; i++) {
            out[i] = b[p + i];
        }
        next = p + len;
    }

    function _bytes32(bytes memory b, uint256 p) private pure returns (bytes32 out, uint256 next) {
        if (p + 32 > b.length) revert Malformed(p);
        assembly {
            out := mload(add(add(b, 32), p))
        }
        next = p + 32;
    }

    function _lengthPrefixed(bytes memory b, uint256 p) private pure returns (bytes memory out, uint256 next) {
        uint256 len;
        (len, p) = _compactU16(b, p);
        return _take(b, p, len);
    }
}
