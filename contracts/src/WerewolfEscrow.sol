// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity ^0.8.24;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {ECDSA} from "@openzeppelin/contracts/utils/cryptography/ECDSA.sol";
import {MerkleProof} from "@openzeppelin/contracts/utils/cryptography/MerkleProof.sol";
import {ReentrancyGuard} from "@openzeppelin/contracts/utils/ReentrancyGuard.sol";
import {IWerewolfErrors} from "./IWerewolfErrors.sol";
import {IRandomnessProvider, IWerewolfEscrow} from "./IWerewolfEscrow.sol";
import {ISP1Verifier} from "./sp1/ISP1Verifier.sol";

/// Escrow and verifiable settlement for staked Agent Werewolf games. See docs/SETTLEMENT.md.
/// @dev Fee-on-transfer and rebasing tokens are unsupported: payouts assume every pull lands in full.
contract WerewolfEscrow is IWerewolfEscrow, IWerewolfErrors, ReentrancyGuard {
    using SafeERC20 for IERC20;

    struct Dispute {
        bytes32 leaf;
        address disputer;
        uint64 deadline;
        bool open;
    }

    struct Game {
        GameView info;
        bool fulfilled;
        bool overturned;
        /// Oldest dispute that may still be open; deadlines grow with the index.
        uint256 firstOpenDispute;
        address[] players;
        bytes32[] sessionKeys;
        Dispute[] disputes;
        mapping(address => uint32) seatPlusOne;
        mapping(bytes32 => bool) leafDisputed;
        mapping(address => uint256) credit;
        /// Bonds of a disputer's open disputes; refunded only if the game ends Void.
        mapping(address => uint256) disputeBonds;
    }

    bytes32 internal constant RECEIPT_TAG = keccak256("werewolf/receipt/v1");
    uint8 internal constant MIN_SEATS = 5;
    uint8 internal constant MAX_SEATS = 12;
    uint8 internal constant MAX_TEAM = 2;
    uint256 internal constant MAX_PAYLOAD = 256;
    uint8 internal constant OP_FORCE = 1;
    uint8 internal constant OP_CHECKPOINT = 2;

    /// Source of the public randomness mixed into each deal.
    IRandomnessProvider public immutable randomnessProvider;
    /// SP1 verifier that checks replay proofs.
    ISP1Verifier public immutable verifier;
    /// Verification key of the SP1 replay program.
    bytes32 public immutable programVKey;
    /// Seconds after `propose` during which anyone may challenge.
    uint64 public immutable challengeWindow;
    /// Seconds after `challenge` the operator has to prove.
    uint64 public immutable proofWindow;
    /// Seconds after `disputeReceipt` the operator has to answer.
    uint64 public immutable disputeWindow;
    /// May pause `create` and `join`, nothing else.
    address public immutable owner;

    /// Whether `create` and `join` are blocked.
    bool public paused;
    /// Id of the latest game; ids start at 1.
    uint256 public gameCount;

    mapping(uint256 => Game) internal _games;

    modifier whenNotPaused() {
        if (paused) revert IsPaused();
        _;
    }

    constructor(
        IRandomnessProvider randomness,
        ISP1Verifier verifier_,
        bytes32 programVKey_,
        uint64 challengeWindow_,
        uint64 proofWindow_,
        uint64 disputeWindow_
    ) {
        if (
            address(randomness) == address(0) || address(verifier_) == address(0)
                || challengeWindow_ == 0 || proofWindow_ == 0 || disputeWindow_ == 0
        ) revert InvalidParams();
        randomnessProvider = randomness;
        verifier = verifier_;
        programVKey = programVKey_;
        challengeWindow = challengeWindow_;
        proofWindow = proofWindow_;
        disputeWindow = disputeWindow_;
        owner = msg.sender;
    }

    function create(CreateParams calldata params)
        external
        nonReentrant
        whenNotPaused
        returns (uint256 gameId)
    {
        if (
            params.token == address(0) || params.stake == 0 || params.minSeats < MIN_SEATS
                || params.minSeats > params.maxSeats || params.maxSeats > MAX_SEATS
                || params.joinBy <= block.timestamp || params.playBy <= params.joinBy
                || params.seedCommit == 0 || params.encKey == 0
        ) revert InvalidParams();

        gameId = ++gameCount;
        GameView storage g = _games[gameId].info;
        g.operator = msg.sender;
        g.token = params.token;
        g.stake = params.stake;
        g.minSeats = params.minSeats;
        g.maxSeats = params.maxSeats;
        g.joinBy = params.joinBy;
        g.playBy = params.playBy;
        g.rulesHash = params.rulesHash;
        g.seedCommit = params.seedCommit;
        g.encKey = params.encKey;
        g.status = Status.Open;

        emit GameCreated(gameId, msg.sender, params);
        IERC20(params.token).safeTransferFrom(msg.sender, address(this), _operatorBond(g));
    }

    function join(uint256 gameId, bytes32 sessionKey) external nonReentrant whenNotPaused {
        Game storage game = _games[gameId];
        GameView storage g = game.info;
        _expect(g, Status.Open);
        if (block.timestamp > g.joinBy) revert TooLate();
        if (g.seatCount >= g.maxSeats) revert GameFull();
        if (game.seatPlusOne[msg.sender] != 0) revert AlreadySeated();
        if (sessionKey == 0) revert InvalidSessionKey();

        uint32 seat = g.seatCount++;
        game.seatPlusOne[msg.sender] = seat + 1;
        game.players.push(msg.sender);
        game.sessionKeys.push(sessionKey);

        emit Joined(gameId, seat, msg.sender, sessionKey);
        IERC20(g.token).safeTransferFrom(msg.sender, address(this), g.stake);
    }

    function start(uint256 gameId) external {
        Game storage game = _games[gameId];
        GameView storage g = game.info;
        _expect(g, Status.Open);
        _onlyOperator(g);
        if (g.seatCount < g.minSeats) revert NotEnoughSeats();

        bytes32 rosterHash = keccak256(abi.encode(game.players, game.sessionKeys));
        g.rosterHash = rosterHash;
        g.status = Status.Started;

        emit Started(gameId, rosterHash);
        // The provider may call `fulfillRandomness` before this returns.
        randomnessProvider.request(gameId);
    }

    function fulfillRandomness(uint256 gameId, bytes32 randomness) external {
        if (msg.sender != address(randomnessProvider)) revert NotProvider();
        Game storage game = _games[gameId];
        _expect(game.info, Status.Started);
        if (game.fulfilled) revert AlreadyFulfilled();

        game.fulfilled = true;
        game.info.randomness = randomness;
        emit RandomnessFulfilled(gameId, randomness);
    }

    function forceAction(uint256 gameId, uint32 round, bytes calldata payload) external {
        Game storage game = _games[gameId];
        GameView storage g = game.info;
        _expectPlaying(game);
        uint32 seatPlusOne = game.seatPlusOne[msg.sender];
        if (seatPlusOne == 0) revert NotSeated();
        if (round != g.nextRound) revert WrongRound();
        if (payload.length == 0 || payload.length > MAX_PAYLOAD) revert InvalidPayload();

        uint32 seat = seatPlusOne - 1;
        bytes32 opLogHash =
            keccak256(abi.encode(g.opLogHash, OP_FORCE, seat, round, keccak256(payload)));
        g.opLogHash = opLogHash;
        emit ActionForced(gameId, round, seat, payload, opLogHash);
    }

    function checkpoint(uint256 gameId, uint32 round) external {
        Game storage game = _games[gameId];
        GameView storage g = game.info;
        _expectPlaying(game);
        _onlyOperator(g);
        if (round != g.nextRound) revert WrongRound();

        bytes32 opLogHash =
            keccak256(abi.encode(g.opLogHash, OP_CHECKPOINT, uint32(0), round, bytes32(0)));
        g.opLogHash = opLogHash;
        g.nextRound = round + 1;
        emit Checkpointed(gameId, round, opLogHash);
    }

    function propose(
        uint256 gameId,
        bytes32 seed,
        uint8 winnerTeam,
        uint256 winners,
        bytes32 transcriptRoot,
        string calldata transcriptUri
    ) external {
        Game storage game = _games[gameId];
        GameView storage g = game.info;
        _expectPlaying(game);
        _onlyOperator(g);
        if (block.timestamp > g.playBy) revert TooLate();
        if (keccak256(abi.encodePacked(seed)) != g.seedCommit) revert InvalidSeed();
        if (winnerTeam > MAX_TEAM) revert InvalidResult();
        _checkWinners(g, winners);

        g.winnerTeam = winnerTeam;
        g.winners = winners;
        g.transcriptRoot = transcriptRoot;
        g.status = Status.Proposed;
        g.challengeEnds = uint64(block.timestamp) + challengeWindow;
        emit Proposed(gameId, seed, winnerTeam, winners, transcriptRoot, transcriptUri);
    }

    function challenge(uint256 gameId) external nonReentrant {
        GameView storage g = _games[gameId].info;
        _expect(g, Status.Proposed);
        if (block.timestamp >= g.challengeEnds) revert TooLate();
        // A proven result is final, so a challenge could only lose its bond.
        if (g.proven) revert AlreadyProven();

        g.status = Status.Challenged;
        g.challenger = msg.sender;
        g.proofDeadline = uint64(block.timestamp) + proofWindow;

        emit Challenged(gameId, msg.sender);
        IERC20(g.token).safeTransferFrom(msg.sender, address(this), g.stake);
    }

    function disputeReceipt(
        uint256 gameId,
        uint64 turnId,
        uint32 seat,
        bytes32 actHash,
        bytes calldata signature
    ) external nonReentrant returns (uint256 disputeId) {
        Game storage game = _games[gameId];
        GameView storage g = game.info;
        if (g.status == Status.Proposed) {
            if (block.timestamp >= g.challengeEnds) revert TooLate();
        } else if (g.status == Status.Challenged) {
            if (block.timestamp >= g.proofDeadline) revert TooLate();
        } else {
            revert WrongStatus(uint8(g.status));
        }
        if (seat >= g.seatCount) revert InvalidSeat();

        bytes32 leaf = _receiptDigest(gameId, turnId, seat, actHash);
        (address signer, ECDSA.RecoverError err,) = ECDSA.tryRecoverCalldata(leaf, signature);
        if (err != ECDSA.RecoverError.NoError || signer != g.operator) revert InvalidSignature();
        if (game.leafDisputed[leaf]) revert DisputeAlreadyOpen();

        uint256 bond = _disputeBond(g);
        disputeId = game.disputes.length;
        game.disputes
            .push(
                Dispute({
                    leaf: leaf,
                    disputer: msg.sender,
                    deadline: uint64(block.timestamp) + disputeWindow,
                    open: true
                })
            );
        game.leafDisputed[leaf] = true;
        game.disputeBonds[msg.sender] += bond;
        g.openDisputes++;

        emit ReceiptDisputed(gameId, disputeId, msg.sender, leaf);
        IERC20(g.token).safeTransferFrom(msg.sender, address(this), bond);
    }

    function answerDispute(uint256 gameId, uint256 disputeId, bytes32[] calldata proof) external {
        Game storage game = _games[gameId];
        GameView storage g = game.info;
        _expectSettling(g);
        if (disputeId >= game.disputes.length || !game.disputes[disputeId].open) {
            revert DisputeNotOpen();
        }
        Dispute storage d = game.disputes[disputeId];
        // Late answers would race `finalize`, which voids the game once a deadline passes.
        if (block.timestamp > d.deadline) revert TooLate();
        if (!MerkleProof.verifyCalldata(proof, g.transcriptRoot, d.leaf)) {
            revert InvalidMerkleProof();
        }

        uint256 bond = _disputeBond(g);
        d.open = false;
        game.leafDisputed[d.leaf] = false;
        game.disputeBonds[d.disputer] -= bond;
        game.credit[g.operator] += bond;
        g.openDisputes--;

        uint256 next = game.firstOpenDispute;
        while (next < game.disputes.length && !game.disputes[next].open) next++;
        game.firstOpenDispute = next;

        emit DisputeAnswered(gameId, disputeId);
    }

    function prove(uint256 gameId, bytes calldata publicValues, bytes calldata proof) external {
        Game storage game = _games[gameId];
        GameView storage g = game.info;
        _expectSettling(g);
        if (g.status == Status.Challenged && block.timestamp > g.proofDeadline) revert TooLate();
        if (g.proven) revert AlreadyProven();

        verifier.verifyProof(programVKey, publicValues, proof);
        PublicValues memory pv = abi.decode(publicValues, (PublicValues));
        if (
            pv.chainId != block.chainid || pv.escrow != address(this) || pv.gameId != gameId
                || pv.rulesHash != g.rulesHash || pv.rosterHash != g.rosterHash
                || pv.seedCommit != g.seedCommit || pv.randomness != g.randomness
                || pv.encKey != g.encKey || pv.opLogHash != g.opLogHash
        ) revert PublicValuesMismatch();
        _checkWinners(g, pv.winners);

        bool matched = pv.winnerTeam == g.winnerTeam && pv.winners == g.winners
            && pv.transcriptRoot == g.transcriptRoot;
        if (!matched) {
            g.winnerTeam = pv.winnerTeam;
            g.winners = pv.winners;
            g.transcriptRoot = pv.transcriptRoot;
            game.overturned = true;
        }
        g.proven = true;
        emit Proved(gameId, matched);
    }

    function finalize(uint256 gameId) external {
        Game storage game = _games[gameId];
        GameView storage g = game.info;
        Status end = _endState(game);
        if (end == Status.None) revert NotFinalizable();

        g.status = end;
        if (end == Status.Cancelled) _payCancelled(game);
        else if (end == Status.Void) _payVoid(game);
        else _paySettled(game);
        emit Finalized(gameId, end);
    }

    function claim(uint256 gameId) external nonReentrant returns (uint256 amount) {
        amount = claimable(gameId, msg.sender);
        if (amount == 0) revert NothingToClaim();

        Game storage game = _games[gameId];
        game.credit[msg.sender] = 0;
        if (game.info.status == Status.Void) game.disputeBonds[msg.sender] = 0;

        emit Claimed(gameId, msg.sender, amount);
        IERC20(game.info.token).safeTransfer(msg.sender, amount);
    }

    function setPaused(bool paused_) external {
        if (msg.sender != owner) revert NotOwner();
        paused = paused_;
        emit PausedSet(paused_);
    }

    function receiptDigest(uint256 gameId, uint64 turnId, uint32 seat, bytes32 actHash)
        external
        view
        returns (bytes32)
    {
        return _receiptDigest(gameId, turnId, seat, actHash);
    }

    function claimable(uint256 gameId, address account) public view returns (uint256 amount) {
        Game storage game = _games[gameId];
        amount = game.credit[account];
        if (game.info.status == Status.Void) amount += game.disputeBonds[account];
    }

    function gameInfo(uint256 gameId) external view returns (GameView memory) {
        return _games[gameId].info;
    }

    function seats(uint256 gameId)
        external
        view
        returns (address[] memory players, bytes32[] memory sessionKeys)
    {
        Game storage game = _games[gameId];
        return (game.players, game.sessionKeys);
    }

    /// End state `finalize` would apply now, or `None` if the game can't end yet.
    function _endState(Game storage game) internal view returns (Status) {
        GameView storage g = game.info;
        Status status = g.status;
        if (status == Status.Open) {
            return block.timestamp > g.joinBy ? Status.Cancelled : Status.None;
        }
        if (status == Status.Started) {
            return block.timestamp > g.playBy ? Status.Void : Status.None;
        }
        if (status != Status.Proposed && status != Status.Challenged) return Status.None;

        if (g.openDisputes > 0 && block.timestamp > game.disputes[game.firstOpenDispute].deadline) {
            return Status.Void;
        }
        if (status == Status.Challenged && !g.proven) {
            return block.timestamp > g.proofDeadline ? Status.Void : Status.None;
        }
        // Even a proven result waits out the dispute window, so receipt holders always get it.
        uint64 disputesClose = status == Status.Challenged ? g.proofDeadline : g.challengeEnds;
        if (g.openDisputes == 0 && block.timestamp >= disputesClose) return Status.Settled;
        return Status.None;
    }

    function _payCancelled(Game storage game) internal {
        GameView storage g = game.info;
        for (uint256 i; i < game.players.length; i++) {
            game.credit[game.players[i]] += g.stake;
        }
        game.credit[g.operator] += _operatorBond(g);
    }

    function _payVoid(Game storage game) internal {
        GameView storage g = game.info;
        for (uint256 i; i < game.players.length; i++) {
            game.credit[game.players[i]] += g.stake;
        }
        _splitAcrossSeats(game, _operatorBond(g));
        if (g.challenger != address(0)) game.credit[g.challenger] += g.stake;
    }

    function _paySettled(Game storage game) internal {
        GameView storage g = game.info;
        uint256 seatCount = game.players.length;
        uint256 pot = uint256(g.stake) * seatCount;
        uint256 winners = g.winners;

        uint256 winnerCount;
        for (uint256 i; i < seatCount; i++) {
            if ((winners >> i) & 1 == 1) winnerCount++;
        }
        uint256 share = pot / winnerCount;
        uint256 dust = pot - share * winnerCount;
        for (uint256 i; i < seatCount; i++) {
            if ((winners >> i) & 1 == 0) continue;
            game.credit[game.players[i]] += share + dust;
            dust = 0;
        }

        uint256 bond = _operatorBond(g);
        if (!game.overturned) {
            game.credit[g.operator] += bond;
            if (g.challenger != address(0)) game.credit[g.operator] += g.stake;
        } else if (g.challenger != address(0)) {
            game.credit[g.challenger] += bond + g.stake;
        } else {
            _splitAcrossSeats(game, bond);
        }
    }

    /// Credits `amount` equally to every seat, with the dust to seat 0.
    function _splitAcrossSeats(Game storage game, uint256 amount) internal {
        uint256 seatCount = game.players.length;
        uint256 share = amount / seatCount;
        for (uint256 i; i < seatCount; i++) {
            game.credit[game.players[i]] += share;
        }
        game.credit[game.players[0]] += amount - share * seatCount;
    }

    function _receiptDigest(uint256 gameId, uint64 turnId, uint32 seat, bytes32 actHash)
        internal
        view
        returns (bytes32)
    {
        return keccak256(
            abi.encode(
                RECEIPT_TAG, uint64(block.chainid), address(this), gameId, turnId, seat, actHash
            )
        );
    }

    function _checkWinners(GameView storage g, uint256 winners) internal view {
        if (winners == 0 || winners >> g.seatCount != 0) revert InvalidResult();
    }

    function _expect(GameView storage g, Status status) internal view {
        if (g.status != status) revert WrongStatus(uint8(g.status));
    }

    /// Started with randomness in: the op log is live and a result may be proposed.
    function _expectPlaying(Game storage game) internal view {
        _expect(game.info, Status.Started);
        if (!game.fulfilled) revert NotFulfilled();
    }

    /// Proposed or Challenged: the result is posted but not final.
    function _expectSettling(GameView storage g) internal view {
        if (g.status != Status.Proposed && g.status != Status.Challenged) {
            revert WrongStatus(uint8(g.status));
        }
    }

    function _onlyOperator(GameView storage g) internal view {
        if (msg.sender != g.operator) revert NotOperator();
    }

    function _operatorBond(GameView storage g) internal view returns (uint256) {
        return uint256(g.stake) * g.maxSeats;
    }

    function _disputeBond(GameView storage g) internal view returns (uint256) {
        uint256 bond = g.stake / 4;
        return bond == 0 ? 1 : bond;
    }
}
