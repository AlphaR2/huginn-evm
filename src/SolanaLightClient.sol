// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {ISP1Verifier} from "./ISP1Verifier.sol";
import {HuginnOutput} from "./HuginnOutput.sol";
import {SolanaTx} from "./SolanaTx.sol";

/// Accepts a Huginn proof that a Solana transaction is in a block finalized by the
/// validator set of that block's epoch. Chain-neutral: the same code runs on Base or any EVM
/// chain, and only the SP1 verifier address differs.
///
/// What the proof already guarantees (checked inside SP1, see `alpenglow-core`):
///   - the BLS aggregate signature(s) verify over the right vote payload,
///   - the signers hold more than the threshold (60% slow path, 80% fast path) of the set,
///   - the block_id is rebuilt from the shreds, and the transaction bytes are in it.
///
/// What this contract adds:
///   - the proof came from our program (`programVKey`) and is valid (SP1 verifier),
///   - the set it was checked against is the one stored for the slot's epoch,
///   - the shred version is this cluster's (mainnet vs testnet vs local).
///
/// Trust assumption, for now: the owner stores each epoch's validator set hash. Proving
/// the set rotation from the previous epoch's set removes the owner (later phase).
contract SolanaLightClient {
    using HuginnOutput for bytes;

    ISP1Verifier public immutable verifier;
    /// The Huginn SP1 program. A new program means a new deployment, never an upgrade.
    bytes32 public immutable programVKey;
    uint16 public immutable shredVersion;
    uint64 public immutable slotsPerEpoch;

    address public owner;
    /// epoch => `validator_set_hash` (alpenglow-core `validator_set_hash`).
    mapping(uint64 => bytes32) public validatorSetHash;
    /// slot => block_id of every block seen finalized.
    mapping(uint64 => bytes32) public finalizedBlock;
    /// keccak256(first signature) => slot, for every transaction submitted.
    mapping(bytes32 => uint64) public transactionSlot;

    event OwnerChanged(address owner);
    event ValidatorSetStored(uint64 indexed epoch, bytes32 setHash);
    event BlockFinalized(uint64 indexed slot, bytes32 blockId);
    event TransactionVerified(bytes32 indexed signatureHash, uint64 indexed slot, bytes32 blockId);

    error NotOwner();
    error UnknownEpoch(uint64 epoch);
    error WrongValidatorSet(bytes32 got, bytes32 expected);
    error WrongShredVersion(uint16 got);
    error SetAlreadyStored(uint64 epoch);
    /// Two different blocks finalized at one slot: a safety failure of Solana itself.
    error ConflictingBlock(uint64 slot, bytes32 stored, bytes32 got);

    constructor(
        ISP1Verifier verifier_,
        bytes32 programVKey_,
        uint16 shredVersion_,
        uint64 slotsPerEpoch_,
        uint64 epoch,
        bytes32 setHash
    ) {
        verifier = verifier_;
        programVKey = programVKey_;
        shredVersion = shredVersion_;
        slotsPerEpoch = slotsPerEpoch_;
        owner = msg.sender;
        emit OwnerChanged(msg.sender);
        _storeSet(epoch, setHash);
    }

    modifier onlyOwner() {
        if (msg.sender != owner) revert NotOwner();
        _;
    }

    function setOwner(address owner_) external onlyOwner {
        owner = owner_;
        emit OwnerChanged(owner_);
    }

    /// Write-once per epoch, so a stolen owner key can't rewrite history already relied on.
    function storeValidatorSet(uint64 epoch, bytes32 setHash) external onlyOwner {
        _storeSet(epoch, setHash);
    }

    function epochOf(uint64 slot) public view returns (uint64) {
        return slot / slotsPerEpoch;
    }

    /// Checks a proof and returns what it proves, without writing anything.
    function verify(bytes calldata publicValues, bytes calldata proof)
        public
        view
        returns (HuginnOutput.Output memory out)
    {
        verifier.verifyProof(programVKey, publicValues, proof);
        out = publicValues.decode();
        if (out.shredVersion != shredVersion) revert WrongShredVersion(out.shredVersion);
        uint64 epoch = epochOf(out.slot);
        bytes32 expected = validatorSetHash[epoch];
        if (expected == bytes32(0)) revert UnknownEpoch(epoch);
        if (out.validatorSetHash != expected) revert WrongValidatorSet(out.validatorSetHash, expected);
    }

    /// `verify`, then records the block and the transaction so other contracts can look
    /// them up later without a proof.
    function submit(bytes calldata publicValues, bytes calldata proof)
        external
        returns (HuginnOutput.Output memory out)
    {
        out = verify(publicValues, proof);
        bytes32 stored = finalizedBlock[out.slot];
        if (stored == bytes32(0)) {
            finalizedBlock[out.slot] = out.blockId;
            emit BlockFinalized(out.slot, out.blockId);
        } else if (stored != out.blockId) {
            revert ConflictingBlock(out.slot, stored, out.blockId);
        }
        bytes32 sigHash = keccak256(SolanaTx.firstSignature(out.transaction));
        transactionSlot[sigHash] = out.slot;
        emit TransactionVerified(sigHash, out.slot, out.blockId);
    }

    /// True once a proof for the transaction with this first signature was submitted.
    function isVerified(bytes calldata signature) external view returns (bool) {
        return transactionSlot[keccak256(signature)] != 0;
    }

    function _storeSet(uint64 epoch, bytes32 setHash) internal {
        if (validatorSetHash[epoch] != bytes32(0)) revert SetAlreadyStored(epoch);
        validatorSetHash[epoch] = setHash;
        emit ValidatorSetStored(epoch, setHash);
    }
}
