// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {SolanaLightClient} from "./SolanaLightClient.sol";
import {HuginnOutput} from "./HuginnOutput.sol";
import {SolanaTx} from "./SolanaTx.sol";

/// A contract controlled by a Solana key. The key's holder sends a Solana transaction with
/// a Memo instruction
///
///     huginn:<chain id>:<target>:<command>
///
/// and anyone relays its Huginn proof here. The contract runs `<command>` once if:
///   - the proof is valid (SolanaLightClient.verify: final, included, right validator set),
///   - the transaction was signed by `controller`,
///   - the memo names this chain and this contract's `target` (no replay on other chains
///     or other contracts),
///   - this transaction was not used before.
///
/// Why a signed-but-failed transaction still counts: Solana includes failed transactions
/// in blocks, and the proof shows inclusion, not success. Authority comes from the
/// controller's signature, which is valid either way. If your command must depend on the
/// Solana side succeeding, prove that separately.
abstract contract SolanaControlled {
    /// MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr
    bytes32 internal constant MEMO_PROGRAM = 0x054a53522854cdd103cc2e4631a3f51792ac727cf1e50abc97828ac5994f3cc2;

    SolanaLightClient public immutable huginn;
    /// The Solana public key (ed25519, 32 bytes) whose transactions control this contract.
    bytes32 public immutable controller;
    /// This contract's name in memos, e.g. "greeter".
    string public target;
    /// keccak256(first signature) of every transaction already executed.
    mapping(bytes32 => bool) public executed;

    event Executed(bytes32 indexed signatureHash, uint64 slot, bytes command);

    error NotSignedByController();
    error NoCommand();
    error AlreadyExecuted();

    constructor(SolanaLightClient huginn_, bytes32 controller_, string memory target_) {
        huginn = huginn_;
        controller = controller_;
        target = target_;
    }

    /// Verifies the proof and runs the transaction's command.
    function execute(bytes calldata publicValues, bytes calldata proof) external {
        HuginnOutput.Output memory out = huginn.verify(publicValues, proof);
        SolanaTx.Transaction memory t = SolanaTx.parse(out.transaction);
        if (!SolanaTx.isSigner(t, controller)) revert NotSignedByController();

        bytes32 id = keccak256(t.signatures[0]);
        if (executed[id]) revert AlreadyExecuted();
        executed[id] = true;

        bytes memory command = _command(t);
        // The only external call above is `huginn.verify`, a view (STATICCALL): it can't
        // re-enter, and `executed[id]` is set before `_execute` runs.
        // forge-lint: disable-next-line(reentrancy-events)
        emit Executed(id, out.slot, command);
        _execute(command);
    }

    /// What `controller` asked for. Implemented by the concrete contract.
    function _execute(bytes memory command) internal virtual;

    /// The first memo addressed to this chain and target, without the prefix.
    function _command(SolanaTx.Transaction memory t) internal view returns (bytes memory) {
        bytes memory prefix = bytes.concat("huginn:", _uint(block.chainid), ":", bytes(target), ":");
        for (uint256 i; i < t.instructions.length; i++) {
            if (SolanaTx.programId(t, i) != MEMO_PROGRAM) continue;
            bytes memory data = t.instructions[i].data;
            if (data.length < prefix.length || keccak256(_slice(data, 0, prefix.length)) != keccak256(prefix)) continue;
            return _slice(data, prefix.length, data.length);
        }
        revert NoCommand();
    }

    function _slice(bytes memory b, uint256 from, uint256 to) private pure returns (bytes memory out) {
        out = new bytes(to - from);
        for (uint256 i; i < out.length; i++) {
            out[i] = b[from + i];
        }
    }

    function _uint(uint256 v) private pure returns (bytes memory s) {
        if (v == 0) return "0";
        while (v != 0) {
            // forge-lint: disable-next-line(unsafe-typecast)
            s = bytes.concat(bytes1(uint8(48 + v % 10)), s); // a digit: 48 + 0..9 fits
            v /= 10;
        }
    }
}
