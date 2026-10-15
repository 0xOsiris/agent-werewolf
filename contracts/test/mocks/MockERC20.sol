// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity ^0.8.24;

import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";

/// Test token anyone can mint.
contract MockERC20 is ERC20 {
    constructor() ERC20("Werewolf Test Token", "WOLF") {}

    function mint(address to, uint256 amount) external {
        _mint(to, amount);
    }
}
