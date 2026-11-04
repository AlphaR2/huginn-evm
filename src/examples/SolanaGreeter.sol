// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {SolanaControlled} from "../SolanaControlled.sol";
import {SolanaLightClient} from "../SolanaLightClient.sol";

/// The smallest Solana-controlled contract: a greeting only a Solana key can change.
///
/// On Solana, the controller sends a memo `huginn:<chain id>:greeter:<new greeting>`.
/// Anyone relays the proof to `execute`, and `greeting` becomes `<new greeting>`.
contract SolanaGreeter is SolanaControlled {
    string public greeting = "gm";

    constructor(SolanaLightClient huginn_, bytes32 controller_) SolanaControlled(huginn_, controller_, "greeter") {}

    function _execute(bytes memory command) internal override {
        greeting = string(command);
    }
}
