// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Script, console} from "forge-std/Script.sol";
import {ISP1Verifier} from "../src/ISP1Verifier.sol";
import {SolanaLightClient} from "../src/SolanaLightClient.sol";
import {IValidatorSetSource} from "../src/sources/IValidatorSetSource.sol";
import {BindingVerifier} from "../src/mocks/BindingVerifier.sol";
import {SP1Verifier} from "../src/sp1/SP1VerifierGroth16.sol";

/// forge script script/Deploy.s.sol --rpc-url $RPC_URL --broadcast
/// Settings come from .env (see .env.example). With no SP1_VERIFIER, on anvil only:
///   LOCAL_VERIFIER=groth16 deploys SP1's real Groth16 verifier (src/sp1), for real proofs;
///   otherwise the stand-in (src/mocks), for practice and STARK proofs.
contract Deploy is Script {
    function run() external returns (SolanaLightClient client) {
        address verifier = vm.envOr("SP1_VERIFIER", address(0));
        uint256 key = vm.envUint("PRIVATE_KEY");
        vm.startBroadcast(key);
        if (verifier == address(0)) {
            require(block.chainid == 31337, "set SP1_VERIFIER: local verifiers are for anvil only");
            if (keccak256(bytes(vm.envOr("LOCAL_VERIFIER", string("")))) == keccak256("groth16")) {
                verifier = address(new SP1Verifier());
                console.log("SP1 Groth16 verifier v6.1.0 (local):", verifier);
            } else {
                verifier = address(new BindingVerifier());
                console.log("stand-in verifier (local only):", verifier);
            }
        }
        client = new SolanaLightClient(
            ISP1Verifier(verifier),
            vm.envBytes32("PROGRAM_VKEY"),
            uint16(vm.envUint("SHRED_VERSION")),
            uint64(vm.envUint("SLOTS_PER_EPOCH")),
            uint64(vm.envUint("EPOCH")),
            vm.envBytes32("VALIDATOR_SET_HASH"),
            setDelay(),
            vm.envOr("WARMUP", false),
            // No proven handover yet: SET_SOURCE stays unset (trusted-owner mode).
            IValidatorSetSource(vm.envOr("SET_SOURCE", address(0))),
            uint64(vm.envOr("REANCHOR_DELAY", uint256(7 days)))
        );
        vm.stopBroadcast();
        console.log("SolanaLightClient:", address(client));
        console.log("validator set delay (s):", client.setDelay());
    }

    /// SET_DELAY from .env. Unset: none on anvil, two hours anywhere else (an epoch is ~1 day;
    /// the app refuses a delay above 1/8 of the measured epoch).
    function setDelay() internal view returns (uint64) {
        return uint64(vm.envOr("SET_DELAY", block.chainid == 31337 ? uint256(0) : uint256(2 hours)));
    }
}
