// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity ^0.8.24;

import {Script, console} from "forge-std/Script.sol";
import {WerewolfEscrow} from "../src/WerewolfEscrow.sol";
import {PrevrandaoRandomness} from "../src/randomness/PrevrandaoRandomness.sol";
import {ISP1Verifier} from "../src/sp1/ISP1Verifier.sol";
import {SP1MockVerifier} from "../src/sp1/SP1MockVerifier.sol";
import {MockERC20} from "../test/mocks/MockERC20.sol";

/// Dev/testnet deployment; prints one JSON line with every address.
contract Deploy is Script {
    function run() external {
        address token = vm.envOr("TOKEN", address(0));
        address verifier = vm.envOr("VERIFIER", address(0));
        bytes32 programVKey = vm.envOr("PROGRAM_VKEY", bytes32(0));
        uint64 challengeWindow = _window("CHALLENGE_WINDOW", 120);
        uint64 proofWindow = _window("PROOF_WINDOW", 600);
        uint64 disputeWindow = _window("DISPUTE_WINDOW", 300);

        vm.startBroadcast();
        if (token == address(0)) token = address(new MockERC20());
        if (verifier == address(0)) verifier = address(new SP1MockVerifier());
        PrevrandaoRandomness randomness = new PrevrandaoRandomness();
        WerewolfEscrow escrow = new WerewolfEscrow(
            randomness,
            ISP1Verifier(verifier),
            programVKey,
            challengeWindow,
            proofWindow,
            disputeWindow
        );
        vm.stopBroadcast();

        console.log(
            string.concat(
                '{"chain_id":',
                vm.toString(block.chainid),
                ',"escrow":"',
                vm.toString(address(escrow)),
                '","token":"',
                vm.toString(token),
                '","randomness":"',
                vm.toString(address(randomness)),
                '","verifier":"',
                vm.toString(verifier),
                '","program_vkey":"',
                vm.toString(programVKey),
                '"}'
            )
        );
    }

    function _window(string memory name, uint256 fallbackSeconds) internal view returns (uint64) {
        uint256 value = vm.envOr(name, fallbackSeconds);
        require(value > 0 && value <= type(uint64).max, string.concat(name, " out of range"));
        return uint64(value);
    }
}
