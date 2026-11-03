// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// Decodes the public values the Huginn SP1 program commits (`TxOutput::encode` in
/// `crates/alpenglow-core/src/proof.rs`). Big-endian, fixed offsets, no ABI encoding, so any
/// chain can read it the same way:
///
///   0   version u8 (= 1)
///   1   validator_set_hash [32]
///   33  shred_version u16
///   35  slot u64
///   43  block_id [32]
///   75  path u8 (0 fast, 1 slow)
///   76  signed_stake u64
///   84  total_stake u64
///   92  component_start_shred u32
///   96  entry_index u32
///   100 tx_index u32
///   104 tx_len u16
///   106 tx bytes [tx_len]
library HuginnOutput {
    uint8 internal constant VERSION = 1;
    uint256 internal constant HEADER_LEN = 106;

    error BadLength(uint256 got);
    error BadVersion(uint8 got);

    struct Output {
        bytes32 validatorSetHash;
        uint16 shredVersion;
        uint64 slot;
        bytes32 blockId;
        bool slowPath;
        uint64 signedStake;
        uint64 totalStake;
        uint32 componentStartShred;
        uint32 entryIndex;
        uint32 txIndex;
        bytes transaction;
    }

    function decode(bytes calldata pv) internal pure returns (Output memory o) {
        if (pv.length < HEADER_LEN) revert BadLength(pv.length);
        if (uint8(pv[0]) != VERSION) revert BadVersion(uint8(pv[0]));
        uint256 txLen = uint16(bytes2(pv[104:106]));
        if (pv.length != HEADER_LEN + txLen) revert BadLength(pv.length);

        o.validatorSetHash = bytes32(pv[1:33]);
        o.shredVersion = uint16(bytes2(pv[33:35]));
        o.slot = uint64(bytes8(pv[35:43]));
        o.blockId = bytes32(pv[43:75]);
        o.slowPath = uint8(pv[75]) == 1;
        o.signedStake = uint64(bytes8(pv[76:84]));
        o.totalStake = uint64(bytes8(pv[84:92]));
        o.componentStartShred = uint32(bytes4(pv[92:96]));
        o.entryIndex = uint32(bytes4(pv[96:100]));
        o.txIndex = uint32(bytes4(pv[100:104]));
        o.transaction = pv[106:];
    }
}
