// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Script, console} from "forge-std/Script.sol";
import {ISP1Verifier} from "../src/ISP1Verifier.sol";
import {SolanaLightClient} from "../src/SolanaLightClient.sol";
import {BindingVerifier} from "../src/mocks/BindingVerifier.sol";

/// forge script script/Deploy.s.sol --rpc-url $RPC_URL --broadcast
/// Settings come from .env (see .env.example).
contract Deploy is Script {
    function run() external returns (SolanaLightClient client) {
        address verifier = vm.envOr("SP1_VERIFIER", address(0));
        uint256 key = vm.envUint("PRIVATE_KEY");
        vm.startBroadcast(key);
        if (verifier == address(0)) {
            require(block.chainid == 31337, "set SP1_VERIFIER: the stand-in verifier is for anvil only");
            verifier = address(new BindingVerifier());
            console.log("stand-in verifier (local only):", verifier);
        }
        client = new SolanaLightClient(
            ISP1Verifier(verifier),
            vm.envBytes32("PROGRAM_VKEY"),
            uint16(vm.envUint("SHRED_VERSION")),
            uint64(vm.envUint("SLOTS_PER_EPOCH")),
            uint64(vm.envUint("EPOCH")),
            vm.envBytes32("VALIDATOR_SET_HASH")
        );
        vm.stopBroadcast();
        console.log("SolanaLightClient:", address(client));
    }
}
