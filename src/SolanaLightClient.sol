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
/// the set rotation from the previous epoch's set removes the owner (later phase). Until
/// then, the owner should be a multisig, and the owner's power is made visible and slow:
///   - a set is first *proposed*, and only counts `setDelay` seconds later. Until then
///     anyone can see it (`pendingSet`, `ValidatorSetProposed`) and the owner can replace or
///     cancel it, so a typo can be fixed before anything relies on it;
///   - a set already in force can be superseded the same way (a typo found late), never
///     instantly;
///   - ownership moves in two steps (`transferOwnership`, then `acceptOwnership` by the new
///     owner), so a mistyped address can't strand the client.
contract SolanaLightClient {
    using HuginnOutput for bytes;

    ISP1Verifier public immutable verifier;
    /// The Huginn SP1 program. A new program means a new deployment, never an upgrade.
    bytes32 public immutable programVKey;
    uint16 public immutable shredVersion;
    uint64 public immutable slotsPerEpoch;

    /// How long a proposed validator set waits before it counts, in seconds.
    uint64 public immutable setDelay;

    address public owner;
    /// Set by `transferOwnership`, becomes `owner` when it calls `acceptOwnership`.
    address public pendingOwner;

    struct Proposal {
        bytes32 setHash;
        uint64 activeAt;
    }

    /// epoch => the set in force (alpenglow-core `validator_set_hash`), not counting a
    /// proposal that has matured but not been settled yet: use `validatorSetHash(epoch)`.
    mapping(uint64 => bytes32) internal _activeSet;
    /// epoch => a proposed set and when it starts to count.
    mapping(uint64 => Proposal) public pendingSet;
    /// slot => block_id of every block seen finalized.
    mapping(uint64 => bytes32) public finalizedBlock;
    /// keccak256(first signature) => slot, for every transaction submitted.
    mapping(bytes32 => uint64) public transactionSlot;

    event OwnerChanged(address owner);
    event OwnershipTransferStarted(address indexed from, address indexed to);
    event ValidatorSetProposed(uint64 indexed epoch, bytes32 setHash, uint64 activeAt);
    event ValidatorSetCancelled(uint64 indexed epoch, bytes32 setHash);
    event ValidatorSetStored(uint64 indexed epoch, bytes32 setHash);
    event BlockFinalized(uint64 indexed slot, bytes32 blockId);
    event TransactionVerified(bytes32 indexed signatureHash, uint64 indexed slot, bytes32 blockId);

    error NotOwner();
    error NotPendingOwner();
    error ZeroSet();
    error NothingPending(uint64 epoch);
    error UnknownEpoch(uint64 epoch);
    error WrongValidatorSet(bytes32 got, bytes32 expected);
    error WrongShredVersion(uint16 got);
    /// Two different blocks finalized at one slot: a safety failure of Solana itself.
    error ConflictingBlock(uint64 slot, bytes32 stored, bytes32 got);

    constructor(
        ISP1Verifier verifier_,
        bytes32 programVKey_,
        uint16 shredVersion_,
        uint64 slotsPerEpoch_,
        uint64 epoch,
        bytes32 setHash,
        uint64 setDelay_
    ) {
        verifier = verifier_;
        programVKey = programVKey_;
        shredVersion = shredVersion_;
        slotsPerEpoch = slotsPerEpoch_;
        setDelay = setDelay_;
        owner = msg.sender;
        emit OwnerChanged(msg.sender);
        // The first set is the deployer's starting point and counts at once: anyone checks
        // it before relying on this client, just like the program key.
        if (setHash == bytes32(0)) revert ZeroSet();
        _activeSet[epoch] = setHash;
        emit ValidatorSetStored(epoch, setHash);
    }

    modifier onlyOwner() {
        if (msg.sender != owner) revert NotOwner();
        _;
    }

    /// Step 1 of 2: names the next owner. Nothing changes until it accepts, so a typo is
    /// harmless: call again with the right address, or with address(0) to cancel.
    function transferOwnership(address to) external onlyOwner {
        pendingOwner = to;
        emit OwnershipTransferStarted(owner, to);
    }

    /// Step 2 of 2, called by the new owner.
    function acceptOwnership() external {
        if (msg.sender != pendingOwner || msg.sender == address(0)) revert NotPendingOwner();
        owner = msg.sender;
        pendingOwner = address(0);
        emit OwnerChanged(msg.sender);
    }

    /// Proposes epoch `epoch`'s validator set. It counts from `setDelay` seconds from now.
    /// Proposing again before then replaces the proposal and restarts the wait. For an
    /// epoch that already has a set, the old one stays in force until the new one counts.
    function proposeValidatorSet(uint64 epoch, bytes32 setHash) external onlyOwner {
        if (setHash == bytes32(0)) revert ZeroSet();
        _settle(epoch);
        uint64 activeAt = uint64(block.timestamp) + setDelay;
        pendingSet[epoch] = Proposal(setHash, activeAt);
        emit ValidatorSetProposed(epoch, setHash, activeAt);
    }

    /// Withdraws a proposal that doesn't count yet.
    function cancelValidatorSet(uint64 epoch) external onlyOwner {
        Proposal memory p = pendingSet[epoch];
        if (p.setHash == bytes32(0) || block.timestamp >= p.activeAt) revert NothingPending(epoch);
        delete pendingSet[epoch];
        emit ValidatorSetCancelled(epoch, p.setHash);
    }

    /// Records a matured proposal as the set in force. Optional: `verify` already uses a
    /// matured proposal. Anyone can call it, for the event.
    function settleValidatorSet(uint64 epoch) external {
        _settle(epoch);
    }

    /// The set proofs for `epoch` must have been checked against: a matured proposal if
    /// there is one, otherwise the set in force. Zero if the epoch has none.
    function validatorSetHash(uint64 epoch) public view returns (bytes32) {
        Proposal memory p = pendingSet[epoch];
        if (p.setHash != bytes32(0) && block.timestamp >= p.activeAt) return p.setHash;
        return _activeSet[epoch];
    }

    function epochOf(uint64 slot) public view returns (uint64) {
        return slot / slotsPerEpoch;
    }

    /// Checks a proof and returns what it proves, without writing anything. Also refuses a
    /// block that conflicts with one already submitted for the same slot.
    function verify(bytes calldata publicValues, bytes calldata proof)
        public
        view
        returns (HuginnOutput.Output memory out)
    {
        verifier.verifyProof(programVKey, publicValues, proof);
        out = publicValues.decode();
        if (out.shredVersion != shredVersion) revert WrongShredVersion(out.shredVersion);
        uint64 epoch = epochOf(out.slot);
        bytes32 expected = validatorSetHash(epoch);
        if (expected == bytes32(0)) revert UnknownEpoch(epoch);
        if (out.validatorSetHash != expected) revert WrongValidatorSet(out.validatorSetHash, expected);
        // Another block_id already recorded for this slot: refuse here too, not only in
        // `submit`, so applications that call `verify` get the same protection.
        bytes32 stored = finalizedBlock[out.slot];
        if (stored != bytes32(0) && stored != out.blockId) revert ConflictingBlock(out.slot, stored, out.blockId);
    }

    /// `verify`, then records the block and the transaction so other contracts can look
    /// them up later without a proof.
    function submit(bytes calldata publicValues, bytes calldata proof)
        external
        returns (HuginnOutput.Output memory out)
    {
        out = verify(publicValues, proof); // reverts on a conflicting block
        if (finalizedBlock[out.slot] == bytes32(0)) {
            finalizedBlock[out.slot] = out.blockId;
            emit BlockFinalized(out.slot, out.blockId);
        }
        bytes32 sigHash = keccak256(SolanaTx.firstSignature(out.transaction));
        transactionSlot[sigHash] = out.slot;
        emit TransactionVerified(sigHash, out.slot, out.blockId);
    }

    /// True once a proof for the transaction with this first signature was submitted.
    function isVerified(bytes calldata signature) external view returns (bool) {
        return transactionSlot[keccak256(signature)] != 0;
    }

    function _settle(uint64 epoch) internal {
        Proposal memory p = pendingSet[epoch];
        if (p.setHash == bytes32(0) || block.timestamp < p.activeAt) return;
        _activeSet[epoch] = p.setHash;
        delete pendingSet[epoch];
        emit ValidatorSetStored(epoch, p.setHash);
    }
}
