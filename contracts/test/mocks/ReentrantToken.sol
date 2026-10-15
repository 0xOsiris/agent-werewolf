// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity ^0.8.24;

import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";

/// Token whose transfers call back into a target, to test reentrancy guards.
contract ReentrantToken is ERC20 {
    address public target;
    bytes public data;

    constructor() ERC20("Reentrant", "RE") {}

    function mint(address to, uint256 amount) external {
        _mint(to, amount);
    }

    function arm(address target_, bytes calldata data_) external {
        target = target_;
        data = data_;
    }

    function _update(address from, address to, uint256 value) internal override {
        super._update(from, to, value);
        address t = target;
        if (t == address(0)) return;
        target = address(0);
        (bool ok, bytes memory ret) = t.call(data);
        if (!ok) {
            assembly ("memory-safe") {
                revert(add(ret, 0x20), mload(ret))
            }
        }
    }
}
