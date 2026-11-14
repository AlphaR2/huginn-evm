// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {ISP1Verifier} from "./ISP1Verifier.sol";
import {HuginnOutput} from "./HuginnOutput.sol";
import {SolanaTx} from "./SolanaTx.sol";
import {IValidatorSetSource} from "./sources/IValidatorSetSource.sol";

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
///   - an optional *guardian* (a hot key or monitoring bot) can only **pause** the client or
///     **cancel** a pending set. It cannot propose or install a set, change any setting, or
///     unpause: only the owner can undo a pause. So a stolen guardian key can stop the client
///     (annoying, visible) but never make it accept anything;
///   - ownership moves in two steps (`transferOwnership`, then `acceptOwnership` by the new
///     owner), so a mistyped address can't strand the client.
contract SolanaLightClient {
    using HuginnOutput for bytes;

    ISP1Verifier public immutable verifier;
    /// The Huginn SP1 program. A new program means a new deployment, never an upgrade.
    bytes32 public immutable programVKey;
    uint16 public immutable shredVersion;
    uint64 public immutable slotsPerEpoch;
    /// Solana's epoch schedule can start with short "warmup" epochs (32, 64, 128, ... slots)
    /// before epochs reach `slotsPerEpoch`. Zero when the cluster has no warmup.
    uint64 public immutable firstNormalEpoch;
    uint64 public immutable firstNormalSlot;

    /// How long a proposed validator set waits before it counts, in seconds.
    uint64 public immutable setDelay;
    /// Proven handover (W8): where proven validator sets come from. Zero = none; the owner then
    /// proposes every set (trusted-owner mode, delay `setDelay`).
    IValidatorSetSource public immutable setSource;
    /// With a set source, the owner may still re-anchor (W4: a missed epoch breaks the proven
    /// chain, since each set must be certified by the previous one). That is a declared trust
    /// event: it waits this much longer delay and the guardian can cancel it.
    uint64 public immutable reanchorDelay;

    address public owner;
    /// Set by `transferOwnership`, becomes `owner` when it calls `acceptOwnership`.
    address public pendingOwner;
    /// Can `pause` and `cancelValidatorSet`, nothing else. Zero = no guardian.
    address public guardian;
    /// While true, `verify` and `submit` revert. Set by owner or guardian, cleared by owner only.
    bool public paused;

    struct Proposal {
        bytes32 setHash;
        uint64 activeAt;
    }

    /// epoch => the set in force (alpenglow-core `validator_set_hash`), not counting a
    /// proposal that has matured but not been settled yet: use `validatorSetHash(epoch)`.
    mapping(uint64 => bytes32) internal _activeSet;
    /// epoch => a proposed set and when it starts to count.
    mapping(uint64 => Proposal) public pendingSet;
    /// slot => validator set hash => block_id of the block seen finalized under that set.
    /// Keyed by the set on purpose: a block recorded while a bad set was in force must not
    /// block the real proof for that slot once the real set is back. Two different blocks
    /// under the SAME set is the real safety failure `ConflictingBlock` reports.
    mapping(uint64 => mapping(bytes32 => bytes32)) public finalizedBlock;
    struct TxRecord {
        bool verified;
        uint64 slot; // slot 0 is a valid slot: never read `slot != 0` as "verified"
    }
    /// validator set hash => keccak256(first signature) => where it was proven under that set.
    /// Keyed by the set for the same reason as `finalizedBlock`: a record made while a bad set
    /// was in force must not block the real proof once the real set is back.
    mapping(bytes32 => mapping(bytes32 => TxRecord)) public txRecord;
    /// keccak256(first signature) => every set hash it was recorded under (usually one).
    mapping(bytes32 => bytes32[]) internal _txSets;

    event OwnerChanged(address owner);
    event GuardianChanged(address indexed guardian);
    event Paused(address indexed by);
    event Unpaused(address indexed by);
    event OwnershipTransferStarted(address indexed from, address indexed to);
    event ValidatorSetProposed(uint64 indexed epoch, bytes32 setHash, uint64 activeAt);
    event ValidatorSetCancelled(uint64 indexed epoch, bytes32 setHash);
    event ValidatorSetStored(uint64 indexed epoch, bytes32 setHash);
    /// A set installed from evidence checked by `setSource`: no owner involved.
    event ValidatorSetProven(uint64 indexed epoch, bytes32 setHash, address source);
    /// The owner re-anchors while a set source exists: a trust event, watch for it.
    event ValidatorSetReanchorProposed(uint64 indexed epoch, bytes32 setHash, uint64 activeAt);
    event BlockFinalized(uint64 indexed slot, bytes32 blockId, bytes32 setHash);
    event TransactionVerified(bytes32 indexed signatureHash, uint64 indexed slot, bytes32 blockId);

    error NotOwner();
    error NotPendingOwner();
    error NotOwnerOrGuardian();
    error ClientPaused();
    /// The same transaction was already recorded in another slot. A transaction is in one
    /// block; two proofs naming different slots mean a safety failure or a bad earlier set.
    error ConflictingTransaction(bytes32 signatureHash, uint64 stored, uint64 got);
    error NotVerified(bytes32 signatureHash);
    error ZeroSet();
    error NoSetSource();
    error ReanchorTooShort();
    /// A proven set disagrees with the one already in force for that epoch.
    error ConflictingSet(uint64 epoch, bytes32 inForce, bytes32 proven);
    error NothingPending(uint64 epoch);
    error UnknownEpoch(uint64 epoch);
    error WrongValidatorSet(bytes32 got, bytes32 expected);
    error WrongShredVersion(uint16 got);
    /// Two different blocks finalized at one slot by the same validator set: a safety failure
    /// of Solana itself.
    error ConflictingBlock(uint64 slot, bytes32 stored, bytes32 got);

    constructor(
        ISP1Verifier verifier_,
        bytes32 programVKey_,
        uint16 shredVersion_,
        uint64 slotsPerEpoch_,
        uint64 epoch,
        bytes32 setHash,
        uint64 setDelay_,
        bool warmup_,
        IValidatorSetSource setSource_,
        uint64 reanchorDelay_
    ) {
        verifier = verifier_;
        programVKey = programVKey_;
        shredVersion = shredVersion_;
        slotsPerEpoch = slotsPerEpoch_;
        if (warmup_) {
            // Solana's EpochSchedule: warmup epochs double from 32 slots until they reach
            // slotsPerEpoch (getEpochSchedule reports the same two numbers).
            uint64 len = MINIMUM_SLOTS_PER_EPOCH;
            uint64 e = 0;
            while (len < slotsPerEpoch_) {
                len <<= 1;
                e++;
            }
            firstNormalEpoch = e;
            firstNormalSlot = len - MINIMUM_SLOTS_PER_EPOCH;
        }
        setDelay = setDelay_;
        setSource = setSource_;
        reanchorDelay = reanchorDelay_;
        if (address(setSource_) != address(0) && reanchorDelay_ <= setDelay_) revert ReanchorTooShort();
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

    /// Names (or, with address(0), removes) the guardian.
    function setGuardian(address guardian_) external onlyOwner {
        guardian = guardian_;
        emit GuardianChanged(guardian_);
    }

    /// Stops `verify` and `submit`. Owner or guardian; never needed to be fast to undo.
    function pause() external {
        if (msg.sender != owner && (guardian == address(0) || msg.sender != guardian)) revert NotOwnerOrGuardian();
        paused = true;
        emit Paused(msg.sender);
    }

    /// Only the owner can resume.
    function unpause() external onlyOwner {
        paused = false;
        emit Unpaused(msg.sender);
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
        bool reanchor = address(setSource) != address(0);
        uint64 activeAt = uint64(block.timestamp) + (reanchor ? reanchorDelay : setDelay);
        pendingSet[epoch] = Proposal(setHash, activeAt);
        emit ValidatorSetProposed(epoch, setHash, activeAt);
        if (reanchor) emit ValidatorSetReanchorProposed(epoch, setHash, activeAt);
    }

    /// Installs `epoch`'s set from evidence that `setSource` checks against the set in force for
    /// `epoch - 1`. Anyone can call it. It counts at once (the evidence is the proof), and it
    /// replaces a pending owner proposal for that epoch. It never overrides a different set
    /// already in force: that conflict is reported, not resolved here.
    function installProvenSet(uint64 epoch, bytes calldata evidence) external returns (bytes32 setHash) {
        if (paused) revert ClientPaused();
        if (address(setSource) == address(0)) revert NoSetSource();
        bytes32 prev = validatorSetHash(epoch - 1);
        if (prev == bytes32(0)) revert UnknownEpoch(epoch - 1);
        setHash = setSource.verifyNextSet(epoch, prev, evidence);
        if (setHash == bytes32(0)) revert ZeroSet();
        _settle(epoch);
        bytes32 inForce = _activeSet[epoch];
        if (inForce != bytes32(0) && inForce != setHash) revert ConflictingSet(epoch, inForce, setHash);
        Proposal memory p = pendingSet[epoch];
        if (p.setHash != bytes32(0)) {
            delete pendingSet[epoch];
            emit ValidatorSetCancelled(epoch, p.setHash);
        }
        _activeSet[epoch] = setHash;
        emit ValidatorSetStored(epoch, setHash);
        emit ValidatorSetProven(epoch, setHash, address(setSource));
    }

    /// Withdraws a proposal that doesn't count yet.
    /// The owner or the guardian can cancel (the guardian's whole point).
    function cancelValidatorSet(uint64 epoch) external {
        if (msg.sender != owner && (guardian == address(0) || msg.sender != guardian)) revert NotOwnerOrGuardian();
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

    uint64 internal constant MINIMUM_SLOTS_PER_EPOCH = 32;

    /// The epoch a slot belongs to, following Solana's EpochSchedule (warmup included).
    function epochOf(uint64 slot) public view returns (uint64) {
        if (slot >= firstNormalSlot) return firstNormalEpoch + (slot - firstNormalSlot) / slotsPerEpoch;
        uint64 start = 0;
        uint64 len = MINIMUM_SLOTS_PER_EPOCH;
        uint64 e = 0;
        while (slot >= start + len) {
            start += len;
            len <<= 1;
            e++;
        }
        return e;
    }

    /// Checks a proof and returns what it proves, without writing anything. Also refuses a
    /// block that conflicts with one already submitted for the same slot.
    function verify(bytes calldata publicValues, bytes calldata proof)
        public
        view
        returns (HuginnOutput.Output memory out)
    {
        if (paused) revert ClientPaused();
        verifier.verifyProof(programVKey, publicValues, proof);
        out = publicValues.decode();
        if (out.shredVersion != shredVersion) revert WrongShredVersion(out.shredVersion);
        uint64 epoch = epochOf(out.slot);
        bytes32 expected = validatorSetHash(epoch);
        if (expected == bytes32(0)) revert UnknownEpoch(epoch);
        if (out.validatorSetHash != expected) revert WrongValidatorSet(out.validatorSetHash, expected);
        // Another block_id already recorded for this slot under this same set: refuse here
        // too, not only in `submit`, so applications that call `verify` get the same
        // protection.
        bytes32 stored = finalizedBlock[out.slot][expected];
        if (stored != bytes32(0) && stored != out.blockId) revert ConflictingBlock(out.slot, stored, out.blockId);
    }

    /// `verify`, then records the block and the transaction so other contracts can look
    /// them up later without a proof.
    function submit(bytes calldata publicValues, bytes calldata proof)
        external
        returns (HuginnOutput.Output memory out)
    {
        out = verify(publicValues, proof); // reverts on a conflicting block
        if (finalizedBlock[out.slot][out.validatorSetHash] == bytes32(0)) {
            finalizedBlock[out.slot][out.validatorSetHash] = out.blockId;
            emit BlockFinalized(out.slot, out.blockId, out.validatorSetHash);
        }
        bytes32 sigHash = keccak256(SolanaTx.firstSignature(out.transaction));
        TxRecord memory r = txRecord[out.validatorSetHash][sigHash];
        if (r.verified) {
            // Resubmitting is harmless (same slot) but never overwrites the first record.
            // Under ONE set, the same transaction in two slots is a safety failure.
            if (r.slot != out.slot) revert ConflictingTransaction(sigHash, r.slot, out.slot);
            return out;
        }
        txRecord[out.validatorSetHash][sigHash] = TxRecord(true, out.slot);
        _txSets[sigHash].push(out.validatorSetHash);
        emit TransactionVerified(sigHash, out.slot, out.blockId);
    }

    /// The record that counts now: one made under the set currently in force for its epoch.
    /// A record made under a set that was later replaced does not count.
    function _currentRecord(bytes32 sigHash) internal view returns (bool found, uint64 slot) {
        bytes32[] storage sets = _txSets[sigHash];
        for (uint256 i = 0; i < sets.length; i++) {
            TxRecord memory r = txRecord[sets[i]][sigHash];
            if (validatorSetHash(epochOf(r.slot)) == sets[i]) return (true, r.slot);
        }
    }

    /// True once a proof for the transaction with this first signature was submitted under
    /// the validator set now in force for its epoch.
    function isVerified(bytes calldata signature) external view returns (bool ok) {
        (ok,) = _currentRecord(keccak256(signature));
    }

    /// The slot of the transaction's current record (see `isVerified`). Reverts if none.
    function transactionSlot(bytes32 signatureHash) external view returns (uint64 slot) {
        bool ok;
        (ok, slot) = _currentRecord(signatureHash);
        if (!ok) revert NotVerified(signatureHash);
    }

    function _settle(uint64 epoch) internal {
        Proposal memory p = pendingSet[epoch];
        if (p.setHash == bytes32(0) || block.timestamp < p.activeAt) return;
        _activeSet[epoch] = p.setHash;
        delete pendingSet[epoch];
        emit ValidatorSetStored(epoch, p.setHash);
    }
}
