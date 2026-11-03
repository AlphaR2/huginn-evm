// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Script, console} from "forge-std/Script.sol";
import {HuginnOutput} from "../src/HuginnOutput.sol";
import {SolanaLightClient} from "../src/SolanaLightClient.sol";
import {BindingVerifier} from "../src/mocks/BindingVerifier.sol";

/// forge script script/Submit.s.sol --rpc-url $RPC_URL --broadcast
/// Submits the proof in $PROOF_FILE (written by the relayer) to $HUGINN_CLIENT.
contract Submit is Script {
    function run() external returns (HuginnOutput.Output memory out) {
        SolanaLightClient client = SolanaLightClient(vm.envAddress("HUGINN_CLIENT"));
        string memory json = vm.readFile(vm.envOr("PROOF_FILE", string("proofs/proof.json")));
        bytes memory pv = vm.parseBytes(string.concat("0x", vm.parseJsonString(json, ".public_values")));
        bytes memory proof = vm.parseBytes(string.concat("0x", vm.parseJsonString(json, ".proof")));
        if (proof.length == 0) {
            // Not a Groth16 proof: only the local stand-in verifier can accept it.
            require(block.chainid == 31337, "PROOF_FILE has no Groth16 proof");
            proof = BindingVerifier(address(client.verifier())).proofFor(client.programVKey(), pv);
        }
        vm.startBroadcast(vm.envUint("PRIVATE_KEY"));
        out = client.submit(pv, proof);
        vm.stopBroadcast();
        console.log("verified Solana slot", out.slot);
        console.logBytes32(out.blockId);
    }
}
