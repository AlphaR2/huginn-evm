// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {SolanaLightClient} from "./SolanaLightClient.sol";
import {HuginnOutput} from "./HuginnOutput.sol";
import {SolanaTx} from "./SolanaTx.sol";

/// A contract controlled by a Solana key. The key's holder sends a Solana transaction with
/// a Memo instruction
///
///     huginn:<chain id>:<this contract's address>:<nonce>:<command>
///
/// for example `huginn:8453:0x5fbdb2315678afecb367f032d93f642f64180aa3:1:hello`, and anyone
/// relays its Huginn proof here. The contract runs `<command>` if:
///   - the proof is valid (SolanaLightClient.verify: final, included, right validator set),
///   - the transaction was signed by `controller`,
///   - the memo names this chain and this contract's address (the address in any letter
///     case), so it can't run on another chain or another deployment,
///   - `<nonce>` is exactly `nonce + 1` (decimal, no leading zeros).
///
/// The nonce gives commands a fixed order: a relayer can delay a command but not reorder
/// them, and each command runs at most once. To cancel a command you signed but that was
/// never relayed, sign an empty command with the same nonce (`...:<nonce>:`) and relay that
/// one first: it uses up the nonce and runs nothing.
///
/// A failed Solana transaction still counts. Solana includes failed transactions in blocks,
/// and the proof shows inclusion, not success. Authority comes from the controller's
/// signature, which is valid either way. If your command must depend on the Solana side
/// succeeding (a payment, a burn), don't build it on this contract: prove that separately.
abstract contract SolanaControlled {
    /// MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr
    bytes32 internal constant MEMO_PROGRAM = 0x054a53522854cdd103cc2e4631a3f51792ac727cf1e50abc97828ac5994f3cc2;

    SolanaLightClient public immutable huginn;
    /// The Solana public key (ed25519, 32 bytes) whose transactions control this contract.
    bytes32 public immutable controller;
    /// The last command's nonce. The next memo must carry `nonce + 1`.
    uint64 public nonce;

    event Executed(uint64 indexed nonce, uint64 slot, bytes command);

    error NotSignedByController();
    error NoCommand();
    error BadNonce();
    error WrongNonce(uint64 got, uint64 expected);

    constructor(SolanaLightClient huginn_, bytes32 controller_) {
        huginn = huginn_;
        controller = controller_;
    }

    /// Verifies the proof and runs the transaction's command.
    function execute(bytes calldata publicValues, bytes calldata proof) external {
        HuginnOutput.Output memory out = huginn.verify(publicValues, proof);
        SolanaTx.Transaction memory t = SolanaTx.parse(out.transaction);
        if (!SolanaTx.isSigner(t, controller)) revert NotSignedByController();

        (uint64 n, bytes memory command) = _command(t);
        if (n != nonce + 1) revert WrongNonce(n, nonce + 1);
        nonce = n;

        // The only external call above is `huginn.verify`, a view (STATICCALL): it can't
        // re-enter, and `nonce` is updated before `_execute` runs.
        // forge-lint: disable-next-line(reentrancy-events)
        emit Executed(n, out.slot, command);
        if (command.length != 0) _execute(command);
    }

    /// The memo prefix this contract answers to, without the nonce:
    /// `huginn:<chain id>:<address, lowercase>:`. Off-chain tools can call it.
    function memoPrefix() public view returns (string memory) {
        return string(bytes.concat("huginn:", _uint(block.chainid), ":", _hex(address(this)), ":"));
    }

    /// What `controller` asked for. Implemented by the concrete contract. Never called with
    /// an empty command (that is a cancel).
    function _execute(bytes memory command) internal virtual;

    /// The first memo addressed to this chain and contract: its nonce and its command.
    function _command(SolanaTx.Transaction memory t) internal view returns (uint64, bytes memory) {
        bytes memory head = bytes(memoPrefix());
        for (uint256 i; i < t.instructions.length; i++) {
            if (SolanaTx.programId(t, i) != MEMO_PROGRAM) continue;
            bytes memory data = t.instructions[i].data;
            if (!_startsWithLower(data, head)) continue;

            uint256 start = head.length;
            uint256 j = start;
            uint256 v = 0;
            while (j < data.length && data[j] >= "0" && data[j] <= "9") {
                v = v * 10 + (uint8(data[j]) - 48);
                j++;
            }
            uint256 digits = j - start;
            // 1 to 19 digits fit in a uint64; no leading zeros, so one nonce has one spelling.
            if (digits == 0 || digits > 19 || (digits > 1 && data[start] == "0")) revert BadNonce();
            if (j == data.length || data[j] != ":") revert BadNonce();
            // forge-lint: disable-next-line(unsafe-typecast)
            return (uint64(v), _slice(data, j + 1, data.length)); // at most 19 digits: fits
        }
        revert NoCommand();
    }

    /// `data` starts with `head`, ignoring the case of ASCII letters in `data`. `head` is
    /// all lowercase.
    function _startsWithLower(bytes memory data, bytes memory head) private pure returns (bool) {
        if (data.length < head.length) return false;
        for (uint256 i; i < head.length; i++) {
            bytes1 c = data[i];
            if (c >= "A" && c <= "Z") c = bytes1(uint8(c) + 32);
            if (c != head[i]) return false;
        }
        return true;
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

    /// `0x` and 40 lowercase hex digits.
    function _hex(address a) private pure returns (bytes memory s) {
        bytes16 digits = "0123456789abcdef";
        s = new bytes(42);
        s[0] = "0";
        s[1] = "x";
        uint160 v = uint160(a);
        for (uint256 i = 41; i > 1; i--) {
            s[i] = digits[v & 0xf];
            v >>= 4;
        }
    }
}
