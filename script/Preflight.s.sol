// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Script, console} from "forge-std/Script.sol";

/// Read-only checks to run before `Deploy.s.sol` on a real chain. Sends nothing.
///
///   EXPECT_CHAIN_ID=84532 forge script script/Preflight.s.sol --rpc-url $RPC_URL
///
/// Reverts with the first problem it finds, and prints what it checked.
contract Preflight is Script {
    // anvil's public test key: its address holds funds only on a local chain
    address constant ANVIL_DEPLOYER = 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266;

    function run() external view {
        uint256 expected = vm.envUint("EXPECT_CHAIN_ID");
        require(block.chainid == expected, "RPC_URL points at a different chain than EXPECT_CHAIN_ID");
        console.log("chain id:", block.chainid);

        address deployer = vm.addr(vm.envUint("PRIVATE_KEY"));
        if (block.chainid != 31337) require(deployer != ANVIL_DEPLOYER, "PRIVATE_KEY is anvil's public test key");
        require(deployer.balance > 0, "deployer has no gas funds on this chain");
        console.log("deployer:", deployer);
        console.log("balance (wei):", deployer.balance);

        address verifier = vm.envOr("SP1_VERIFIER", address(0));
        if (block.chainid != 31337) {
            require(verifier != address(0), "SP1_VERIFIER is empty: set Succinct's gateway for this chain");
            require(verifier.code.length > 0, "SP1_VERIFIER has no code on this chain");
        }
        console.log("SP1 verifier:", verifier);

        require(vm.envBytes32("PROGRAM_VKEY") != bytes32(0), "PROGRAM_VKEY is empty: run `huginn-evm vkey`");
        require(vm.envBytes32("VALIDATOR_SET_HASH") != bytes32(0), "VALIDATOR_SET_HASH is empty");
        uint256 shred = vm.envUint("SHRED_VERSION");
        require(shred != 0 && shred <= type(uint16).max, "SHRED_VERSION out of range");
        require(vm.envUint("SLOTS_PER_EPOCH") > 0, "SLOTS_PER_EPOCH is zero");
        console.log("shred version:", shred);

        uint256 delay = vm.envOr("SET_DELAY", uint256(1 days));
        require(
            delay > 0 || block.chainid == 31337, "SET_DELAY is 0 on a real chain: validator sets would count at once"
        );
        console.log("set delay (s):", delay);
        console.log("OK: nothing sent. Review the values above, then run Deploy.s.sol.");
    }
}
