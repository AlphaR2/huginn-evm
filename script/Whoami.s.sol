// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Script, console} from "forge-std/Script.sol";

/// Prints who PRIVATE_KEY is on this chain and what it holds. Sends nothing. The key is read
/// from the environment, so it never appears on a command line.
contract Whoami is Script {
    function run() external view {
        address who = vm.addr(vm.envUint("PRIVATE_KEY"));
        console.log("chain id:", block.chainid);
        console.log("deployer:", who);
        console.log("balance wei:", who.balance);
        console.log("block:", block.number);
    }
}
