// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Script, console} from "forge-std/Script.sol";
import {SolanaLightClient} from "../src/SolanaLightClient.sol";
import {SolanaGreeter} from "../src/examples/SolanaGreeter.sol";

/// forge script script/DeployGreeter.s.sol --rpc-url $RPC_URL --broadcast
/// Deploys SolanaGreeter on $HUGINN_CLIENT, controlled by the Solana key $SOLANA_CONTROLLER
/// (32 bytes hex: the ed25519 public key behind a base58 address).
contract DeployGreeter is Script {
    function run() external returns (SolanaGreeter greeter) {
        vm.startBroadcast(vm.envUint("PRIVATE_KEY"));
        greeter =
            new SolanaGreeter(SolanaLightClient(vm.envAddress("HUGINN_CLIENT")), vm.envBytes32("SOLANA_CONTROLLER"));
        vm.stopBroadcast();
        console.log("SolanaGreeter:", address(greeter));
    }
}
