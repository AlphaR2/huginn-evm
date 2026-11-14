// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// Where a proven validator set comes from (the *handover*: learning epoch N's set from data
/// Solana produced, certified by a set the light client already trusts). One interface, two
/// sources, matching huginn's `crates/handover`:
///   - A. `RecorderSetSource`: a recorder program's account plus the epoch-boundary block's
///     account changes (not built yet);
///   - B. `EpochStakesSetSource`: SIMD-0676's on-chain epoch-stakes account (waits for the
///     proposal).
/// Swapping A for B means deploying the other source; the light client does not change.
interface IValidatorSetSource {
    /// Checks `evidence` and returns the commitment (alpenglow-core `validator_set_hash`) of
    /// `epoch`'s set. `trustedPrev` is the set the light client holds for `epoch - 1`: the
    /// evidence must be certified by it. Reverts if anything does not check out.
    function verifyNextSet(uint64 epoch, bytes32 trustedPrev, bytes calldata evidence)
        external
        view
        returns (bytes32 setHash);
}
