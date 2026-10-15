// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity ^0.8.24;

/// Transcript-root Merkle tree from docs/SETTLEMENT.md: sorted pairs, odd last node moves up.
library Merkle {
    function hashPair(bytes32 a, bytes32 b) internal pure returns (bytes32) {
        return a < b ? keccak256(abi.encodePacked(a, b)) : keccak256(abi.encodePacked(b, a));
    }

    function root(bytes32[] memory leaves) internal pure returns (bytes32) {
        if (leaves.length == 0) return bytes32(0);
        bytes32[] memory level = leaves;
        while (level.length > 1) {
            level = _up(level);
        }
        return level[0];
    }

    function proof(bytes32[] memory leaves, uint256 index)
        internal
        pure
        returns (bytes32[] memory path)
    {
        path = new bytes32[](32);
        uint256 len;
        bytes32[] memory level = leaves;
        while (level.length > 1) {
            uint256 sibling = index ^ 1;
            if (sibling < level.length) path[len++] = level[sibling];
            level = _up(level);
            index /= 2;
        }
        assembly ("memory-safe") {
            mstore(path, len)
        }
    }

    function _up(bytes32[] memory level) private pure returns (bytes32[] memory next) {
        next = new bytes32[]((level.length + 1) / 2);
        for (uint256 i; i < level.length / 2; i++) {
            next[i] = hashPair(level[2 * i], level[2 * i + 1]);
        }
        if (level.length % 2 == 1) next[next.length - 1] = level[level.length - 1];
    }
}
