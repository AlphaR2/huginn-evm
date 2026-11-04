// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Script, console} from "forge-std/Script.sol";
import {SolanaGreeter} from "../src/examples/SolanaGreeter.sol";

/// forge script script/Execute.s.sol --rpc-url $RPC_URL --broadcast
/// Relays a proven Solana command to a SolanaControlled contract. Reads GREETER,
/// PUBLIC_VALUES, PROOF and PRIVATE_KEY from the environment (the key never goes on a
/// command line, where other users of the machine could see it).
contract Execute is Script {
    function run() external {
        SolanaGreeter greeter = SolanaGreeter(vm.envAddress("GREETER"));
        vm.startBroadcast(vm.envUint("PRIVATE_KEY"));
        greeter.execute(vm.envBytes("PUBLIC_VALUES"), vm.envBytes("PROOF"));
        vm.stopBroadcast();
        console.log("nonce now", greeter.nonce());
        console.log("greeting:", greeter.greeting());
    }
}
