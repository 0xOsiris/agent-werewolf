// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity ^0.8.24;

import {IRandomnessProvider, IWerewolfEscrow} from "../IWerewolfEscrow.sol";

/// Dev/testnet randomness that calls back the requester at once.
/// @dev Block proposers can grind prevrandao, so this is for testnets only, never real stakes.
contract PrevrandaoRandomness is IRandomnessProvider {
    function request(uint256 gameId) external {
        bytes32 value = keccak256(abi.encode(block.prevrandao, gameId, block.number));
        IWerewolfEscrow(msg.sender).fulfillRandomness(gameId, value);
    }
}
