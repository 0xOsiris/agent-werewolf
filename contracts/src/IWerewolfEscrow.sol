// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity ^0.8.24;

/// Escrow and verifiable settlement for staked Agent Werewolf games. See docs/SETTLEMENT.md.
interface IWerewolfEscrow {
    enum Status {
        None,
        Open,
        Started,
        Proposed,
        Challenged,
        Settled,
        Void,
        Cancelled
    }

    struct CreateParams {
        address token;
        uint96 stake;
        uint8 minSeats;
        uint8 maxSeats;
        uint64 joinBy;
        uint64 playBy;
        bytes32 rulesHash;
        bytes32 seedCommit;
        bytes32 encKey;
    }

    /// ABI-encoded output of the SP1 program; `prove` checks every field against storage.
    struct PublicValues {
        uint64 chainId;
        address escrow;
        uint256 gameId;
        bytes32 rulesHash;
        bytes32 rosterHash;
        bytes32 seedCommit;
        bytes32 randomness;
        bytes32 encKey;
        bytes32 opLogHash;
        uint8 winnerTeam;
        uint256 winners;
        bytes32 transcriptRoot;
    }

    struct GameView {
        address operator;
        address token;
        uint96 stake;
        uint8 minSeats;
        uint8 maxSeats;
        uint64 joinBy;
        uint64 playBy;
        bytes32 rulesHash;
        bytes32 seedCommit;
        bytes32 encKey;
        Status status;
        uint32 seatCount;
        bytes32 rosterHash;
        bytes32 randomness;
        bytes32 opLogHash;
        uint32 nextRound;
        uint8 winnerTeam;
        uint256 winners;
        bytes32 transcriptRoot;
        uint64 challengeEnds;
        uint64 proofDeadline;
        address challenger;
        bool proven;
        uint32 openDisputes;
    }

    event GameCreated(uint256 indexed gameId, address indexed operator, CreateParams params);
    event Joined(uint256 indexed gameId, uint32 seat, address indexed player, bytes32 sessionKey);
    event Started(uint256 indexed gameId, bytes32 rosterHash);
    event RandomnessFulfilled(uint256 indexed gameId, bytes32 randomness);
    event ActionForced(
        uint256 indexed gameId, uint32 indexed round, uint32 seat, bytes payload, bytes32 opLogHash
    );
    event Checkpointed(uint256 indexed gameId, uint32 indexed round, bytes32 opLogHash);
    event Proposed(
        uint256 indexed gameId,
        bytes32 seed,
        uint8 winnerTeam,
        uint256 winners,
        bytes32 transcriptRoot,
        string transcriptUri
    );
    event Challenged(uint256 indexed gameId, address indexed challenger);
    event ReceiptDisputed(
        uint256 indexed gameId, uint256 indexed disputeId, address indexed disputer, bytes32 leaf
    );
    event DisputeAnswered(uint256 indexed gameId, uint256 indexed disputeId);
    event Proved(uint256 indexed gameId, bool matchedProposal);
    event Finalized(uint256 indexed gameId, Status status);
    event Claimed(uint256 indexed gameId, address indexed account, uint256 amount);
    event PausedSet(bool paused);

    function create(CreateParams calldata params) external returns (uint256 gameId);
    function join(uint256 gameId, bytes32 sessionKey) external;
    function start(uint256 gameId) external;
    function fulfillRandomness(uint256 gameId, bytes32 randomness) external;
    function forceAction(uint256 gameId, uint32 round, bytes calldata payload) external;
    function checkpoint(uint256 gameId, uint32 round) external;
    function propose(
        uint256 gameId,
        bytes32 seed,
        uint8 winnerTeam,
        uint256 winners,
        bytes32 transcriptRoot,
        string calldata transcriptUri
    ) external;
    function challenge(uint256 gameId) external;
    function disputeReceipt(
        uint256 gameId,
        uint64 turnId,
        uint32 seat,
        bytes32 actHash,
        bytes calldata signature
    ) external returns (uint256 disputeId);
    function answerDispute(uint256 gameId, uint256 disputeId, bytes32[] calldata proof) external;
    function prove(uint256 gameId, bytes calldata publicValues, bytes calldata proof) external;
    function finalize(uint256 gameId) external;
    function claim(uint256 gameId) external returns (uint256 amount);
    function setPaused(bool paused) external;

    function receiptDigest(uint256 gameId, uint64 turnId, uint32 seat, bytes32 actHash)
        external
        view
        returns (bytes32);
    function claimable(uint256 gameId, address account) external view returns (uint256);
    function gameInfo(uint256 gameId) external view returns (GameView memory);
    function seats(uint256 gameId)
        external
        view
        returns (address[] memory players, bytes32[] memory sessionKeys);
}

/// Supplies the public randomness mixed into the deal; calls back `fulfillRandomness`.
interface IRandomnessProvider {
    function request(uint256 gameId) external;
}
