// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity ^0.8.24;

import {IRandomnessProvider, IWerewolfEscrow} from "../../src/IWerewolfEscrow.sol";

/// Records requests; tests deliver randomness by hand.
contract MockRandomness is IRandomnessProvider {
    mapping(uint256 => bool) public requested;

    function request(uint256 gameId) external {
        requested[gameId] = true;
    }

    function fulfill(IWerewolfEscrow escrow, uint256 gameId, bytes32 value) external {
        escrow.fulfillRandomness(gameId, value);
    }
}
