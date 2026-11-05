// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Script, console} from "forge-std/Script.sol";
import {SolanaLightClient} from "../src/SolanaLightClient.sol";

/// forge script script/ProposeValidatorSet.s.sol --rpc-url $RPC_URL --broadcast
/// Proposes EPOCH's VALIDATOR_SET_HASH on HUGINN_CLIENT, as its owner (PRIVATE_KEY). The set
/// counts after the client's delay; until then anyone can check it, and proposing again
/// replaces it. With a multisig owner, send the same call from the multisig instead.
contract ProposeValidatorSet is Script {
    function run() external {
        SolanaLightClient client = SolanaLightClient(vm.envAddress("HUGINN_CLIENT"));
        uint64 epoch = uint64(vm.envUint("EPOCH"));
        bytes32 setHash = vm.envBytes32("VALIDATOR_SET_HASH");
        vm.startBroadcast(vm.envUint("PRIVATE_KEY"));
        client.proposeValidatorSet(epoch, setHash);
        vm.stopBroadcast();
        (, uint64 activeAt) = client.pendingSet(epoch);
        console.log("proposed set for epoch", epoch);
        console.log("counts from unix time", activeAt);
    }
}
