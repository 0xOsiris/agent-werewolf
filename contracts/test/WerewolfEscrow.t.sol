// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity ^0.8.24;

import {IRandomnessProvider, IWerewolfEscrow} from "../src/IWerewolfEscrow.sol";
import {IWerewolfErrors} from "../src/IWerewolfErrors.sol";
import {WerewolfEscrow} from "../src/WerewolfEscrow.sol";
import {PrevrandaoRandomness} from "../src/randomness/PrevrandaoRandomness.sol";
import {ISP1Verifier} from "../src/sp1/ISP1Verifier.sol";
import {EscrowBase} from "./Base.t.sol";
import {Merkle} from "./utils/Merkle.sol";
import {ReentrantToken} from "./mocks/ReentrantToken.sol";
import {ReentrancyGuard} from "@openzeppelin/contracts/utils/ReentrancyGuard.sol";

contract WerewolfEscrowTest is EscrowBase {
    using Merkle for bytes32[];

    // ---------------------------------------------------------------- happy path

    function test_HappyPath() public {
        uint256 id = _create();
        assertEq(id, 1);
        assertEq(token.balanceOf(address(escrow)), uint256(STAKE) * MAX_SEATS);

        _join(id, 6);
        vm.prank(operator);
        escrow.start(id);
        assertTrue(rng.requested(id));
        rng.fulfill(escrow, id, RANDOMNESS);
        vm.prank(operator);
        escrow.checkpoint(id, 0);

        uint256 winners = (1 << 1) | (1 << 4);
        _propose(id, winners, ROOT);
        IWerewolfEscrow.GameView memory g = _info(id);
        assertEq(uint8(g.status), uint8(IWerewolfEscrow.Status.Proposed));
        assertEq(g.challengeEnds, block.timestamp + CHALLENGE_WINDOW);

        vm.expectRevert(IWerewolfErrors.NotFinalizable.selector);
        escrow.finalize(id);
        vm.warp(g.challengeEnds);
        escrow.finalize(id);
        assertEq(uint8(_status(id)), uint8(IWerewolfEscrow.Status.Settled));

        assertEq(escrow.claimable(id, players[1]), 3 * uint256(STAKE));
        assertEq(escrow.claimable(id, players[4]), 3 * uint256(STAKE));
        assertEq(escrow.claimable(id, players[0]), 0);
        assertEq(escrow.claimable(id, operator), uint256(STAKE) * MAX_SEATS);

        uint256 before = token.balanceOf(players[1]);
        vm.prank(players[1]);
        assertEq(escrow.claim(id), 3 * uint256(STAKE));
        assertEq(token.balanceOf(players[1]) - before, 3 * uint256(STAKE));
        _claimAll(id, _everyone());
        assertEq(token.balanceOf(address(escrow)), 0);
    }

    function test_SettledDustGoesToLowestWinner() public {
        IWerewolfEscrow.CreateParams memory p = _params();
        p.stake = 10;
        uint256 id = _create(p);
        _join(id, 7);
        _start(id);
        _propose(id, (1 << 2) | (1 << 3) | (1 << 6), ROOT);
        vm.warp(block.timestamp + CHALLENGE_WINDOW);
        escrow.finalize(id);
        // pot 70 over 3 winners: 23 each, dust 1 to seat 2.
        assertEq(escrow.claimable(id, players[2]), 24);
        assertEq(escrow.claimable(id, players[3]), 23);
        assertEq(escrow.claimable(id, players[6]), 23);
    }

    function test_SeatsAndRosterHash() public {
        uint256 id = _create();
        _join(id, 5);
        (address[] memory seatPlayers, bytes32[] memory keys) = escrow.seats(id);
        assertEq(seatPlayers.length, 5);
        for (uint256 i; i < 5; i++) {
            assertEq(seatPlayers[i], players[i]);
            assertEq(keys[i], _sessionKey(i));
        }
        vm.prank(operator);
        escrow.start(id);
        assertEq(_info(id).rosterHash, keccak256(abi.encode(seatPlayers, keys)));
        assertEq(_info(id).seatCount, 5);
    }

    // ---------------------------------------------------------------- constructor and create

    function test_Constructor_RejectsZeroConfig() public {
        IRandomnessProvider r = rng;
        ISP1Verifier v = verifier;
        vm.expectRevert(IWerewolfErrors.InvalidParams.selector);
        new WerewolfEscrow(IRandomnessProvider(address(0)), v, VKEY, 1, 1, 1);
        vm.expectRevert(IWerewolfErrors.InvalidParams.selector);
        new WerewolfEscrow(r, ISP1Verifier(address(0)), VKEY, 1, 1, 1);
        vm.expectRevert(IWerewolfErrors.InvalidParams.selector);
        new WerewolfEscrow(r, v, VKEY, 0, 1, 1);
        vm.expectRevert(IWerewolfErrors.InvalidParams.selector);
        new WerewolfEscrow(r, v, VKEY, 1, 0, 1);
        vm.expectRevert(IWerewolfErrors.InvalidParams.selector);
        new WerewolfEscrow(r, v, VKEY, 1, 1, 0);
    }

    function test_Create_StoresParams() public {
        IWerewolfEscrow.CreateParams memory p = _params();
        vm.expectEmit(address(escrow));
        emit IWerewolfEscrow.GameCreated(1, operator, p);
        uint256 id = _create(p);
        IWerewolfEscrow.GameView memory g = _info(id);
        assertEq(g.operator, operator);
        assertEq(g.token, address(token));
        assertEq(g.stake, STAKE);
        assertEq(g.minSeats, 5);
        assertEq(g.maxSeats, MAX_SEATS);
        assertEq(g.joinBy, p.joinBy);
        assertEq(g.playBy, p.playBy);
        assertEq(g.rulesHash, p.rulesHash);
        assertEq(g.seedCommit, p.seedCommit);
        assertEq(g.encKey, ENC_KEY);
        assertEq(uint8(g.status), uint8(IWerewolfEscrow.Status.Open));
        assertEq(_create(), 2);
        assertEq(escrow.gameCount(), 2);
    }

    function test_Create_RejectsInvalidParams() public {
        for (uint256 i; i < 10; i++) {
            IWerewolfEscrow.CreateParams memory p = _params();
            if (i == 0) p.token = address(0);
            if (i == 1) p.stake = 0;
            if (i == 2) p.minSeats = 4;
            if (i == 3) p.minSeats = p.maxSeats + 1;
            if (i == 4) p.maxSeats = 13;
            if (i == 5) p.joinBy = uint64(block.timestamp);
            if (i == 6) p.playBy = p.joinBy;
            if (i == 7) p.seedCommit = 0;
            if (i == 8) p.encKey = 0;
            if (i == 9) (p.minSeats, p.maxSeats) = (12, 12);
            if (i == 9) {
                _create(p); // 5 <= minSeats <= maxSeats <= 12 holds at the edge.
                continue;
            }
            vm.prank(operator);
            vm.expectRevert(IWerewolfErrors.InvalidParams.selector);
            escrow.create(p);
        }
    }

    function test_Create_RevertsWithoutBondAllowance() public {
        vm.prank(operator);
        token.approve(address(escrow), uint256(STAKE) * MAX_SEATS - 1);
        vm.prank(operator);
        vm.expectRevert();
        escrow.create(_params());
    }

    // ---------------------------------------------------------------- join and start

    function test_Join_Reverts() public {
        vm.prank(players[0]);
        vm.expectRevert(_wrongStatus(IWerewolfEscrow.Status.None));
        escrow.join(99, _sessionKey(0));

        uint256 id = _create();
        vm.prank(players[0]);
        vm.expectRevert(IWerewolfErrors.InvalidSessionKey.selector);
        escrow.join(id, 0);

        _join(id, 1);
        vm.prank(players[0]);
        vm.expectRevert(IWerewolfErrors.AlreadySeated.selector);
        escrow.join(id, _sessionKey(9));

        for (uint256 i = 1; i < MAX_SEATS; i++) {
            vm.prank(players[i]);
            escrow.join(id, _sessionKey(i));
        }
        vm.prank(players[MAX_SEATS]);
        vm.expectRevert(IWerewolfErrors.GameFull.selector);
        escrow.join(id, _sessionKey(MAX_SEATS));

        uint256 late = _create();
        vm.warp(_info(late).joinBy);
        _join(late, 1); // joinBy itself is still open
        vm.warp(block.timestamp + 1);
        vm.prank(players[1]);
        vm.expectRevert(IWerewolfErrors.TooLate.selector);
        escrow.join(late, _sessionKey(1));

        uint256 started = _startedGame(5);
        vm.prank(players[6]);
        vm.expectRevert(_wrongStatus(IWerewolfEscrow.Status.Started));
        escrow.join(started, _sessionKey(6));
    }

    function test_Start_Reverts() public {
        uint256 id = _create();
        _join(id, 4);
        vm.prank(players[0]);
        vm.expectRevert(IWerewolfErrors.NotOperator.selector);
        escrow.start(id);
        vm.prank(operator);
        vm.expectRevert(IWerewolfErrors.NotEnoughSeats.selector);
        escrow.start(id);

        vm.prank(players[4]);
        escrow.join(id, _sessionKey(4));
        _start(id);
        vm.prank(operator);
        vm.expectRevert(_wrongStatus(IWerewolfEscrow.Status.Started));
        escrow.start(id);
    }

    function test_FulfillRandomness_Reverts() public {
        uint256 id = _create();
        _join(id, 5);
        vm.expectRevert(IWerewolfErrors.NotProvider.selector);
        escrow.fulfillRandomness(id, RANDOMNESS);
        vm.expectRevert(_wrongStatus(IWerewolfEscrow.Status.Open));
        rng.fulfill(escrow, id, RANDOMNESS);

        vm.prank(operator);
        escrow.start(id);
        rng.fulfill(escrow, id, 0); // zero is a valid value; a flag tracks fulfillment
        vm.expectRevert(IWerewolfErrors.AlreadyFulfilled.selector);
        rng.fulfill(escrow, id, RANDOMNESS);
        assertEq(_info(id).randomness, 0);
    }

    function test_PrevrandaoRandomness_CallsBackAtOnce() public {
        PrevrandaoRandomness provider = new PrevrandaoRandomness();
        escrow = new WerewolfEscrow(
            provider, verifier, VKEY, CHALLENGE_WINDOW, PROOF_WINDOW, DISPUTE_WINDOW
        );
        _fund(operator);
        for (uint256 i; i < 5; i++) {
            _fund(players[i]);
        }
        vm.prevrandao(bytes32(uint256(42)));
        uint256 id = _create();
        _join(id, 5);
        vm.prank(operator);
        escrow.start(id);
        assertEq(
            _info(id).randomness, keccak256(abi.encode(bytes32(uint256(42)), id, block.number))
        );
        vm.prank(operator);
        escrow.checkpoint(id, 0); // only works once randomness is in
    }

    // ---------------------------------------------------------------- op log

    function test_OpLogHashing() public {
        uint256 id = _startedGame(6);
        bytes memory a = hex"aa01";
        bytes memory b = hex"bb02";
        bytes memory c = hex"cc03";

        bytes32 h = keccak256(abi.encode(bytes32(0), uint8(1), uint32(3), uint32(0), keccak256(a)));
        vm.expectEmit(address(escrow));
        emit IWerewolfEscrow.ActionForced(id, 0, 3, a, h);
        vm.prank(players[3]);
        escrow.forceAction(id, 0, a);

        h = keccak256(abi.encode(h, uint8(2), uint32(0), uint32(0), bytes32(0)));
        vm.expectEmit(address(escrow));
        emit IWerewolfEscrow.Checkpointed(id, 0, h);
        vm.prank(operator);
        escrow.checkpoint(id, 0);

        h = keccak256(abi.encode(h, uint8(1), uint32(1), uint32(1), keccak256(b)));
        vm.prank(players[1]);
        escrow.forceAction(id, 1, b);
        h = keccak256(abi.encode(h, uint8(1), uint32(1), uint32(1), keccak256(c)));
        vm.prank(players[1]);
        escrow.forceAction(id, 1, c);
        h = keccak256(abi.encode(h, uint8(2), uint32(0), uint32(1), bytes32(0)));
        vm.prank(operator);
        escrow.checkpoint(id, 1);

        assertEq(_info(id).opLogHash, h);
        assertEq(_info(id).nextRound, 2);

        // The proof must commit to this op log.
        _propose(id, 1, ROOT);
        _prove(id, 1, 1, ROOT);
        assertTrue(_info(id).proven);
    }

    function test_ForceAction_Reverts() public {
        uint256 id = _create();
        _join(id, 5);
        vm.prank(players[0]);
        vm.expectRevert(_wrongStatus(IWerewolfEscrow.Status.Open));
        escrow.forceAction(id, 0, hex"01");

        vm.prank(operator);
        escrow.start(id);
        vm.prank(players[0]);
        vm.expectRevert(IWerewolfErrors.NotFulfilled.selector);
        escrow.forceAction(id, 0, hex"01");
        rng.fulfill(escrow, id, RANDOMNESS);

        vm.prank(outsider);
        vm.expectRevert(IWerewolfErrors.NotSeated.selector);
        escrow.forceAction(id, 0, hex"01");
        vm.prank(players[0]);
        vm.expectRevert(IWerewolfErrors.WrongRound.selector);
        escrow.forceAction(id, 1, hex"01");
        vm.prank(players[0]);
        vm.expectRevert(IWerewolfErrors.InvalidPayload.selector);
        escrow.forceAction(id, 0, "");
        vm.prank(players[0]);
        vm.expectRevert(IWerewolfErrors.InvalidPayload.selector);
        escrow.forceAction(id, 0, new bytes(257));
        vm.prank(players[0]);
        escrow.forceAction(id, 0, new bytes(256));

        vm.prank(operator);
        escrow.checkpoint(id, 0);
        vm.prank(players[0]);
        vm.expectRevert(IWerewolfErrors.WrongRound.selector);
        escrow.forceAction(id, 0, hex"01");

        _propose(id, 1, ROOT);
        vm.prank(players[0]);
        vm.expectRevert(_wrongStatus(IWerewolfEscrow.Status.Proposed));
        escrow.forceAction(id, 1, hex"01");
    }

    function test_Checkpoint_Reverts() public {
        uint256 id = _create();
        _join(id, 5);
        vm.prank(operator);
        vm.expectRevert(_wrongStatus(IWerewolfEscrow.Status.Open));
        escrow.checkpoint(id, 0);

        vm.prank(operator);
        escrow.start(id);
        vm.prank(operator);
        vm.expectRevert(IWerewolfErrors.NotFulfilled.selector);
        escrow.checkpoint(id, 0);
        rng.fulfill(escrow, id, RANDOMNESS);

        vm.prank(players[0]);
        vm.expectRevert(IWerewolfErrors.NotOperator.selector);
        escrow.checkpoint(id, 0);
        vm.prank(operator);
        vm.expectRevert(IWerewolfErrors.WrongRound.selector);
        escrow.checkpoint(id, 1);
        vm.prank(operator);
        escrow.checkpoint(id, 0);
        vm.prank(operator);
        vm.expectRevert(IWerewolfErrors.WrongRound.selector);
        escrow.checkpoint(id, 0);

        _propose(id, 1, ROOT);
        vm.prank(operator);
        vm.expectRevert(_wrongStatus(IWerewolfEscrow.Status.Proposed));
        escrow.checkpoint(id, 1);
    }

    // ---------------------------------------------------------------- propose

    function test_Propose_Reverts() public {
        uint256 id = _create();
        _join(id, 5);
        vm.prank(operator);
        vm.expectRevert(_wrongStatus(IWerewolfEscrow.Status.Open));
        escrow.propose(id, SEED, 0, 1, ROOT, "");

        vm.prank(operator);
        escrow.start(id);
        vm.prank(operator);
        vm.expectRevert(IWerewolfErrors.NotFulfilled.selector);
        escrow.propose(id, SEED, 0, 1, ROOT, "");
        rng.fulfill(escrow, id, RANDOMNESS);

        vm.prank(players[0]);
        vm.expectRevert(IWerewolfErrors.NotOperator.selector);
        escrow.propose(id, SEED, 0, 1, ROOT, "");
        vm.prank(operator);
        vm.expectRevert(IWerewolfErrors.InvalidSeed.selector);
        escrow.propose(id, bytes32(uint256(SEED) ^ 1), 0, 1, ROOT, "");
        vm.prank(operator);
        vm.expectRevert(IWerewolfErrors.InvalidResult.selector);
        escrow.propose(id, SEED, 3, 1, ROOT, "");
        vm.prank(operator);
        vm.expectRevert(IWerewolfErrors.InvalidResult.selector);
        escrow.propose(id, SEED, 0, 0, ROOT, "");
        vm.prank(operator);
        vm.expectRevert(IWerewolfErrors.InvalidResult.selector);
        escrow.propose(id, SEED, 0, 1 << 5, ROOT, "");

        uint64 playBy = _info(id).playBy;
        vm.warp(playBy + 1);
        vm.prank(operator);
        vm.expectRevert(IWerewolfErrors.TooLate.selector);
        escrow.propose(id, SEED, 0, 1, ROOT, "");

        vm.warp(playBy);
        vm.prank(operator);
        escrow.propose(id, SEED, 2, (1 << 5) - 1, ROOT, "");
        vm.prank(operator);
        vm.expectRevert(_wrongStatus(IWerewolfEscrow.Status.Proposed));
        escrow.propose(id, SEED, 0, 1, ROOT, "");
    }

    // ---------------------------------------------------------------- challenge and prove

    function test_Challenge_Reverts() public {
        uint256 id = _startedGame(5);
        vm.prank(outsider);
        vm.expectRevert(_wrongStatus(IWerewolfEscrow.Status.Started));
        escrow.challenge(id);

        _propose(id, 1, ROOT);
        vm.prank(outsider);
        escrow.challenge(id);
        vm.prank(players[0]);
        vm.expectRevert(_wrongStatus(IWerewolfEscrow.Status.Challenged));
        escrow.challenge(id);

        uint256 late = _proposedGame(5, 1);
        vm.warp(_info(late).challengeEnds);
        vm.prank(outsider);
        vm.expectRevert(IWerewolfErrors.TooLate.selector);
        escrow.challenge(late);

        uint256 proven = _proposedGame(5, 1);
        _prove(proven, 1, 1, ROOT);
        vm.prank(outsider);
        vm.expectRevert(IWerewolfErrors.AlreadyProven.selector);
        escrow.challenge(proven);
    }

    function test_ChallengeWithMatchingProof() public {
        uint256 id = _proposedGame(6, 1 << 2);
        uint256 before = token.balanceOf(outsider);
        vm.prank(outsider);
        escrow.challenge(id);
        assertEq(before - token.balanceOf(outsider), STAKE);
        IWerewolfEscrow.GameView memory g = _info(id);
        assertEq(uint8(g.status), uint8(IWerewolfEscrow.Status.Challenged));
        assertEq(g.challenger, outsider);
        assertEq(g.proofDeadline, block.timestamp + PROOF_WINDOW);

        vm.expectEmit(address(escrow));
        emit IWerewolfEscrow.Proved(id, true);
        _prove(id, 1, 1 << 2, ROOT);

        vm.warp(g.proofDeadline);
        escrow.finalize(id);
        assertEq(uint8(_status(id)), uint8(IWerewolfEscrow.Status.Settled));
        assertEq(escrow.claimable(id, operator), uint256(STAKE) * (MAX_SEATS + 1));
        assertEq(escrow.claimable(id, outsider), 0);
        assertEq(escrow.claimable(id, players[2]), uint256(STAKE) * 6);
        _claimAll(id, _everyone());
        assertEq(token.balanceOf(address(escrow)), 0);
    }

    function test_ChallengeWithMismatchingProof() public {
        uint256 id = _proposedGame(6, 1 << 2);
        vm.prank(outsider);
        escrow.challenge(id);

        vm.expectEmit(address(escrow));
        emit IWerewolfEscrow.Proved(id, false);
        _prove(id, 0, (1 << 0) | (1 << 5), OTHER_ROOT);
        IWerewolfEscrow.GameView memory g = _info(id);
        assertEq(g.winnerTeam, 0);
        assertEq(g.winners, (1 << 0) | (1 << 5));
        assertEq(g.transcriptRoot, OTHER_ROOT);

        vm.warp(g.proofDeadline);
        escrow.finalize(id);
        assertEq(escrow.claimable(id, outsider), uint256(STAKE) * (MAX_SEATS + 1));
        assertEq(escrow.claimable(id, operator), 0);
        assertEq(escrow.claimable(id, players[0]), uint256(STAKE) * 3);
        assertEq(escrow.claimable(id, players[5]), uint256(STAKE) * 3);
        assertEq(escrow.claimable(id, players[2]), 0);
        _claimAll(id, _everyone());
        assertEq(token.balanceOf(address(escrow)), 0);
    }

    function test_RootOnlyMismatchStillOverturns() public {
        uint256 id = _proposedGame(5, 1);
        vm.prank(outsider);
        escrow.challenge(id);
        _prove(id, 1, 1, OTHER_ROOT);
        vm.warp(_info(id).proofDeadline);
        escrow.finalize(id);
        assertEq(escrow.claimable(id, outsider), uint256(STAKE) * (MAX_SEATS + 1));
    }

    function test_UnchallengedCorrectingProofSplitsBond() public {
        IWerewolfEscrow.CreateParams memory p = _params();
        p.stake = 7;
        uint256 id = _create(p);
        _join(id, 5);
        _start(id);
        _propose(id, 1 << 1, ROOT);
        _prove(id, 0, 1 << 3, ROOT);

        vm.expectRevert(IWerewolfErrors.NotFinalizable.selector);
        escrow.finalize(id);
        vm.warp(_info(id).challengeEnds);
        escrow.finalize(id);
        // Bond 56 over 5 seats: 11 each, dust 1 to seat 0. Pot 35 to seat 3.
        assertEq(escrow.claimable(id, players[0]), 12);
        assertEq(escrow.claimable(id, players[1]), 11);
        assertEq(escrow.claimable(id, players[3]), 11 + 35);
        assertEq(escrow.claimable(id, operator), 0);
        _claimAll(id, _everyone());
        assertEq(token.balanceOf(address(escrow)), 0);
    }

    function test_ProposedLateProofStillOverturns() public {
        uint256 id = _proposedGame(5, 1);
        vm.warp(_info(id).challengeEnds + 1 days);
        _prove(id, 0, 2, ROOT);
        escrow.finalize(id);
        assertEq(escrow.claimable(id, players[1]), uint256(STAKE) * 5 + uint256(STAKE) * 8 / 5);
    }

    function test_ChallengeWithoutProofVoids() public {
        uint256 id = _proposedGame(5, 1);
        vm.prank(outsider);
        escrow.challenge(id);
        uint64 deadline = _info(id).proofDeadline;

        vm.warp(deadline);
        vm.expectRevert(IWerewolfErrors.NotFinalizable.selector);
        escrow.finalize(id);
        bytes memory pv = abi.encode(_publicValues(id, 1, 1, ROOT));
        vm.warp(deadline + 1);
        vm.expectRevert(IWerewolfErrors.TooLate.selector);
        escrow.prove(id, pv, "");

        escrow.finalize(id);
        assertEq(uint8(_status(id)), uint8(IWerewolfEscrow.Status.Void));
        // Each seat: stake + bond/5 = 10 + 16.
        for (uint256 i; i < 5; i++) {
            assertEq(escrow.claimable(id, players[i]), uint256(STAKE) * 13 / 5);
        }
        assertEq(escrow.claimable(id, outsider), STAKE);
        assertEq(escrow.claimable(id, operator), 0);
        _claimAll(id, _everyone());
        assertEq(token.balanceOf(address(escrow)), 0);
    }

    function test_Prove_Reverts() public {
        uint256 id = _startedGame(5);
        bytes memory early = abi.encode(_publicValues(id, 1, 1, ROOT));
        vm.expectRevert(_wrongStatus(IWerewolfEscrow.Status.Started));
        escrow.prove(id, early, "");

        _propose(id, 1, ROOT);
        bytes memory good = abi.encode(_publicValues(id, 1, 1, ROOT));
        vm.expectRevert(); // the mock verifier only accepts empty proofs
        escrow.prove(id, good, hex"01");
        vm.expectRevert();
        escrow.prove(id, hex"1234", "");

        for (uint256 field; field < 9; field++) {
            IWerewolfEscrow.PublicValues memory pv = _publicValues(id, 1, 1, ROOT);
            if (field == 0) pv.chainId += 1;
            if (field == 1) pv.escrow = address(1);
            if (field == 2) pv.gameId += 1;
            if (field == 3) pv.rulesHash = bytes32(uint256(pv.rulesHash) ^ 1);
            if (field == 4) pv.rosterHash = bytes32(uint256(pv.rosterHash) ^ 1);
            if (field == 5) pv.seedCommit = bytes32(uint256(pv.seedCommit) ^ 1);
            if (field == 6) pv.randomness = bytes32(uint256(pv.randomness) ^ 1);
            if (field == 7) pv.encKey = bytes32(uint256(pv.encKey) ^ 1);
            if (field == 8) pv.opLogHash = bytes32(uint256(pv.opLogHash) ^ 1);
            vm.expectRevert(IWerewolfErrors.PublicValuesMismatch.selector);
            escrow.prove(id, abi.encode(pv), "");
        }
        bytes memory noWinners = abi.encode(_publicValues(id, 1, 0, ROOT));
        vm.expectRevert(IWerewolfErrors.InvalidResult.selector);
        escrow.prove(id, noWinners, "");
        bytes memory outOfRange = abi.encode(_publicValues(id, 1, 1 << 5, ROOT));
        vm.expectRevert(IWerewolfErrors.InvalidResult.selector);
        escrow.prove(id, outOfRange, "");

        escrow.prove(id, good, "");
        vm.expectRevert(IWerewolfErrors.AlreadyProven.selector);
        escrow.prove(id, good, "");
    }

    function test_ProofIsBoundToChain() public {
        uint256 id = _proposedGame(5, 1);
        bytes memory pv = abi.encode(_publicValues(id, 1, 1, ROOT));
        vm.chainId(block.chainid + 1);
        vm.expectRevert(IWerewolfErrors.PublicValuesMismatch.selector);
        escrow.prove(id, pv, "");
    }

    // ---------------------------------------------------------------- disputes

    function test_DisputeAnswered() public {
        uint256 id = _startedGame(5);
        bytes32[] memory leaves = new bytes32[](5);
        for (uint256 i; i < 5; i++) {
            leaves[i] =
                escrow.receiptDigest(id, uint64(10 + i), uint32(i), keccak256(abi.encode(i)));
        }
        _propose(id, 1, leaves.root());

        uint256 before = token.balanceOf(players[2]);
        uint256 disputeId = _dispute(players[2], id, 12, 2, keccak256(abi.encode(uint256(2))));
        assertEq(disputeId, 0);
        assertEq(before - token.balanceOf(players[2]), STAKE / 4);
        assertEq(_info(id).openDisputes, 1);

        vm.warp(_info(id).challengeEnds);
        vm.expectRevert(IWerewolfErrors.NotFinalizable.selector);
        escrow.finalize(id);

        vm.expectRevert(IWerewolfErrors.InvalidMerkleProof.selector);
        escrow.answerDispute(id, disputeId, leaves.proof(1));
        vm.expectEmit(address(escrow));
        emit IWerewolfEscrow.DisputeAnswered(id, disputeId);
        vm.prank(outsider); // anyone can answer with the public transcript
        escrow.answerDispute(id, disputeId, leaves.proof(2));
        assertEq(_info(id).openDisputes, 0);
        assertEq(escrow.claimable(id, operator), STAKE / 4);
        vm.expectRevert(IWerewolfErrors.DisputeNotOpen.selector);
        escrow.answerDispute(id, disputeId, leaves.proof(2));

        escrow.finalize(id);
        assertEq(uint8(_status(id)), uint8(IWerewolfEscrow.Status.Settled));
        assertEq(escrow.claimable(id, operator), uint256(STAKE) * MAX_SEATS + STAKE / 4);
        assertEq(escrow.claimable(id, players[2]), 0);
        _claimAll(id, _everyone());
        assertEq(token.balanceOf(address(escrow)), 0);
    }

    function test_DisputeUnansweredVoids() public {
        uint256 id = _proposedGame(6, 1);
        uint256 disputeId = _dispute(players[3], id, 7, 3, keccak256("dropped"));
        uint64 deadline = uint64(block.timestamp) + DISPUTE_WINDOW;

        vm.warp(deadline);
        vm.expectRevert(IWerewolfErrors.NotFinalizable.selector);
        escrow.finalize(id);
        vm.warp(deadline + 1);
        vm.expectRevert(IWerewolfErrors.TooLate.selector);
        escrow.answerDispute(id, disputeId, new bytes32[](0));

        escrow.finalize(id);
        assertEq(uint8(_status(id)), uint8(IWerewolfEscrow.Status.Void));
        assertEq(escrow.claimable(id, players[3]), uint256(STAKE) * 14 / 6 + STAKE / 4);
        assertEq(escrow.claimable(id, players[0]), uint256(STAKE) * 14 / 6 + 2);
        assertEq(escrow.claimable(id, operator), 0);
        vm.expectRevert(_wrongStatus(IWerewolfEscrow.Status.Void));
        escrow.answerDispute(id, disputeId, new bytes32[](0));
        _claimAll(id, _everyone());
        assertEq(token.balanceOf(address(escrow)), 0);
    }

    function test_DisputeVoidsEvenAfterProof() public {
        uint256 id = _proposedGame(5, 1);
        vm.prank(outsider);
        escrow.challenge(id);
        _dispute(players[0], id, 1, 0, keccak256("dropped"));
        _prove(id, 0, 2, OTHER_ROOT);
        vm.warp(block.timestamp + DISPUTE_WINDOW + 1);
        escrow.finalize(id);
        assertEq(uint8(_status(id)), uint8(IWerewolfEscrow.Status.Void));
        // Void ignores the proof: the challenger only gets its bond back.
        assertEq(escrow.claimable(id, outsider), STAKE);
        _claimAll(id, _everyone());
        assertEq(token.balanceOf(address(escrow)), 0);
    }

    function test_DisputesOutOfOrder() public {
        uint256 id = _startedGame(5);
        bytes32[] memory leaves = new bytes32[](3);
        for (uint256 i; i < 3; i++) {
            leaves[i] = escrow.receiptDigest(id, uint64(i), uint32(i), bytes32(i));
        }
        _propose(id, 1, leaves.root());
        uint256 first = _dispute(players[0], id, 0, 0, bytes32(0));
        vm.warp(block.timestamp + 10);
        uint256 second = _dispute(players[1], id, 1, 1, bytes32(uint256(1)));
        uint256 third = _dispute(players[2], id, 2, 2, bytes32(uint256(2)));
        assertEq(second, 1);
        assertEq(third, 2);

        escrow.answerDispute(id, second, leaves.proof(1));
        escrow.answerDispute(id, first, leaves.proof(0));
        // Only the third (deadline t+10+window) remains open.
        vm.warp(block.timestamp + DISPUTE_WINDOW);
        vm.expectRevert(IWerewolfErrors.NotFinalizable.selector);
        escrow.finalize(id);
        vm.warp(block.timestamp + 1);
        escrow.finalize(id);
        assertEq(uint8(_status(id)), uint8(IWerewolfEscrow.Status.Void));
        assertEq(escrow.claimable(id, operator), 2 * (STAKE / 4));
        _claimAll(id, _everyone());
        assertEq(token.balanceOf(address(escrow)), 0);
    }

    function test_DisputeReceipt_Reverts() public {
        uint256 id = _startedGame(5);
        (, bytes memory sig) = _receipt(id, 1, 0, bytes32(0));
        vm.expectRevert(_wrongStatus(IWerewolfEscrow.Status.Started));
        escrow.disputeReceipt(id, 1, 0, bytes32(0), sig);

        _propose(id, 1, ROOT);
        (, bytes memory badSeat) = _receipt(id, 1, 5, bytes32(0));
        vm.expectRevert(IWerewolfErrors.InvalidSeat.selector);
        escrow.disputeReceipt(id, 1, 5, bytes32(0), badSeat);

        uint256 strangerKey = uint256(keccak256("stranger"));
        bytes memory stranger = _sign(strangerKey, escrow.receiptDigest(id, 1, 0, bytes32(0)));
        vm.expectRevert(IWerewolfErrors.InvalidSignature.selector);
        escrow.disputeReceipt(id, 1, 0, bytes32(0), stranger);
        vm.expectRevert(IWerewolfErrors.InvalidSignature.selector);
        escrow.disputeReceipt(id, 1, 0, bytes32(0), _slice(sig, 64));
        vm.expectRevert(IWerewolfErrors.InvalidSignature.selector);
        escrow.disputeReceipt(id, 1, 0, bytes32(0), _malleate(sig));
        // A receipt is only valid for the exact (turn, seat, act) it signs.
        vm.expectRevert(IWerewolfErrors.InvalidSignature.selector);
        escrow.disputeReceipt(id, 2, 0, bytes32(0), sig);

        vm.prank(players[0]);
        escrow.disputeReceipt(id, 1, 0, bytes32(0), sig);
        vm.prank(players[1]);
        vm.expectRevert(IWerewolfErrors.DisputeAlreadyOpen.selector);
        escrow.disputeReceipt(id, 1, 0, bytes32(0), sig);

        vm.warp(_info(id).challengeEnds);
        (, bytes memory sig2) = _receipt(id, 2, 0, bytes32(0));
        vm.expectRevert(IWerewolfErrors.TooLate.selector);
        escrow.disputeReceipt(id, 2, 0, bytes32(0), sig2);
    }

    function test_DisputeWindowWhileChallenged() public {
        uint256 id = _proposedGame(5, 1);
        vm.prank(outsider);
        escrow.challenge(id);
        vm.warp(_info(id).challengeEnds); // past the challenge window, inside the proof window
        _dispute(players[0], id, 1, 0, bytes32(0));

        vm.warp(_info(id).proofDeadline);
        (, bytes memory sig) = _receipt(id, 2, 0, bytes32(0));
        vm.prank(players[0]);
        vm.expectRevert(IWerewolfErrors.TooLate.selector);
        escrow.disputeReceipt(id, 2, 0, bytes32(0), sig);
    }

    function test_ReceiptFromOtherGameRejected() public {
        uint256 a = _proposedGame(5, 1);
        uint256 b = _proposedGame(5, 1);
        (, bytes memory sigForB) = _receipt(b, 1, 0, bytes32(0));
        vm.expectRevert(IWerewolfErrors.InvalidSignature.selector);
        escrow.disputeReceipt(a, 1, 0, bytes32(0), sigForB);
    }

    function test_ReceiptDigestIsDomainBound() public {
        bytes32 digest = escrow.receiptDigest(1, 7, 2, keccak256("act"));
        assertEq(
            digest,
            keccak256(
                abi.encode(
                    keccak256("werewolf/receipt/v1"),
                    uint64(block.chainid),
                    address(escrow),
                    uint256(1),
                    uint64(7),
                    uint32(2),
                    keccak256("act")
                )
            )
        );
        assertNotEq(digest, escrow.receiptDigest(2, 7, 2, keccak256("act")));
        WerewolfEscrow other = new WerewolfEscrow(rng, verifier, VKEY, 1, 1, 1);
        assertNotEq(digest, other.receiptDigest(1, 7, 2, keccak256("act")));
        vm.chainId(block.chainid + 1);
        assertNotEq(digest, escrow.receiptDigest(1, 7, 2, keccak256("act")));
    }

    function test_AnswerDispute_Reverts() public {
        uint256 id = _startedGame(5);
        vm.expectRevert(_wrongStatus(IWerewolfEscrow.Status.Started));
        escrow.answerDispute(id, 0, new bytes32[](0));
        _propose(id, 1, ROOT);
        vm.expectRevert(IWerewolfErrors.DisputeNotOpen.selector);
        escrow.answerDispute(id, 0, new bytes32[](0));
    }

    function test_SelfProveThenFinalizeWaitsForChallengeEnds() public {
        uint256 id = _proposedGame(5, 1);
        bytes memory pv = abi.encode(_publicValues(id, 1, 1, ROOT));
        vm.prank(operator);
        escrow.prove(id, pv, "");
        uint64 challengeEnds = _info(id).challengeEnds;

        vm.expectRevert(IWerewolfErrors.NotFinalizable.selector);
        escrow.finalize(id);
        vm.warp(challengeEnds - 1);
        vm.expectRevert(IWerewolfErrors.NotFinalizable.selector);
        escrow.finalize(id);
        vm.warp(challengeEnds);
        escrow.finalize(id);
        assertEq(uint8(_status(id)), uint8(IWerewolfEscrow.Status.Settled));
        assertEq(escrow.claimable(id, operator), uint256(STAKE) * MAX_SEATS);
    }

    function test_ProvenResultWaitsForDisputeWindow() public {
        uint256 id = _proposedGame(5, 1);
        _prove(id, 1, 1, ROOT);
        vm.expectRevert(IWerewolfErrors.NotFinalizable.selector);
        escrow.finalize(id);
        // Receipt holders can still dispute after the proof.
        _dispute(players[0], id, 1, 0, bytes32(0));
        vm.warp(_info(id).challengeEnds);
        vm.expectRevert(IWerewolfErrors.NotFinalizable.selector);
        escrow.finalize(id);
    }

    // ---------------------------------------------------------------- finalize, cancel, void

    function test_Cancelled() public {
        uint256 id = _create();
        _join(id, 6);
        vm.warp(_info(id).joinBy);
        vm.expectRevert(IWerewolfErrors.NotFinalizable.selector);
        escrow.finalize(id);
        vm.warp(block.timestamp + 1);
        vm.expectEmit(address(escrow));
        emit IWerewolfEscrow.Finalized(id, IWerewolfEscrow.Status.Cancelled);
        escrow.finalize(id);
        for (uint256 i; i < 6; i++) {
            assertEq(escrow.claimable(id, players[i]), STAKE);
        }
        assertEq(escrow.claimable(id, operator), uint256(STAKE) * MAX_SEATS);
        _claimAll(id, _everyone());
        assertEq(token.balanceOf(address(escrow)), 0);

        uint256 empty = _create();
        vm.warp(_info(empty).joinBy + 1);
        escrow.finalize(empty);
        vm.prank(operator);
        assertEq(escrow.claim(empty), uint256(STAKE) * MAX_SEATS);
    }

    function test_VoidAtPlayBy() public {
        uint256 id = _create();
        _join(id, 7);
        vm.prank(operator);
        escrow.start(id); // randomness never arrives
        uint64 playBy = _info(id).playBy;
        vm.warp(playBy);
        vm.expectRevert(IWerewolfErrors.NotFinalizable.selector);
        escrow.finalize(id);
        vm.warp(playBy + 1);
        escrow.finalize(id);
        assertEq(uint8(_status(id)), uint8(IWerewolfEscrow.Status.Void));
        // Bond 80 ether over 7 seats; dust to seat 0.
        uint256 share = uint256(STAKE) * MAX_SEATS / 7;
        uint256 dust = uint256(STAKE) * MAX_SEATS - share * 7;
        assertEq(escrow.claimable(id, players[0]), STAKE + share + dust);
        assertEq(escrow.claimable(id, players[6]), STAKE + share);
        _claimAll(id, _everyone());
        assertEq(token.balanceOf(address(escrow)), 0);
    }

    function test_Finalize_NotFinalizable() public {
        vm.expectRevert(IWerewolfErrors.NotFinalizable.selector);
        escrow.finalize(42);
        uint256 id = _create();
        vm.expectRevert(IWerewolfErrors.NotFinalizable.selector);
        escrow.finalize(id);
        _join(id, 5);
        _start(id);
        vm.expectRevert(IWerewolfErrors.NotFinalizable.selector);
        escrow.finalize(id);
        _propose(id, 1, ROOT);
        vm.expectRevert(IWerewolfErrors.NotFinalizable.selector);
        escrow.finalize(id);
        vm.prank(outsider);
        escrow.challenge(id);
        vm.expectRevert(IWerewolfErrors.NotFinalizable.selector);
        escrow.finalize(id);
        _prove(id, 1, 1, ROOT);
        vm.expectRevert(IWerewolfErrors.NotFinalizable.selector);
        escrow.finalize(id);
        vm.warp(_info(id).proofDeadline);
        escrow.finalize(id);
        vm.expectRevert(IWerewolfErrors.NotFinalizable.selector);
        escrow.finalize(id);
    }

    // ---------------------------------------------------------------- claims and admin

    function test_DoubleClaim() public {
        uint256 id = _proposedGame(5, 1);
        vm.warp(_info(id).challengeEnds);
        escrow.finalize(id);
        vm.prank(players[0]);
        escrow.claim(id);
        vm.prank(players[0]);
        vm.expectRevert(IWerewolfErrors.NothingToClaim.selector);
        escrow.claim(id);
        vm.prank(players[1]);
        vm.expectRevert(IWerewolfErrors.NothingToClaim.selector);
        escrow.claim(id);
    }

    function test_OperatorAlsoPlays() public {
        uint256 id = _create();
        vm.prank(operator);
        escrow.join(id, _sessionKey(99));
        _join(id, 4);
        _start(id);
        // Operator (seat 0) and seat 2 (players[1]) win.
        _propose(id, (1 << 0) | (1 << 2), ROOT);
        vm.warp(_info(id).challengeEnds);
        escrow.finalize(id);
        uint256 expected = uint256(STAKE) * MAX_SEATS + uint256(STAKE) * 5 / 2;
        assertEq(escrow.claimable(id, operator), expected);
        uint256 before = token.balanceOf(operator);
        vm.prank(operator);
        escrow.claim(id);
        assertEq(token.balanceOf(operator) - before, expected);
        _claimAll(id, _everyone());
        assertEq(token.balanceOf(address(escrow)), 0);
    }

    function test_Pause() public {
        uint256 id = _create();
        vm.prank(players[0]);
        vm.expectRevert(IWerewolfErrors.NotOwner.selector);
        escrow.setPaused(true);

        vm.expectEmit(address(escrow));
        emit IWerewolfEscrow.PausedSet(true);
        escrow.setPaused(true);
        vm.prank(operator);
        vm.expectRevert(IWerewolfErrors.IsPaused.selector);
        escrow.create(_params());
        vm.prank(players[0]);
        vm.expectRevert(IWerewolfErrors.IsPaused.selector);
        escrow.join(id, _sessionKey(0));

        escrow.setPaused(false);
        _join(id, 5);
        escrow.setPaused(true);
        // Everything past join keeps working while paused.
        _start(id);
        _propose(id, 1, ROOT);
        vm.warp(_info(id).challengeEnds);
        escrow.finalize(id);
        _claimAll(id, _everyone());
        assertEq(token.balanceOf(address(escrow)), 0);
    }

    function test_ClaimIsNonReentrant() public {
        ReentrantToken bad = new ReentrantToken();
        for (uint256 i; i < 5; i++) {
            bad.mint(players[i], STAKE);
            vm.prank(players[i]);
            bad.approve(address(escrow), STAKE);
        }
        bad.mint(operator, uint256(STAKE) * MAX_SEATS);
        vm.prank(operator);
        bad.approve(address(escrow), type(uint256).max);
        IWerewolfEscrow.CreateParams memory p = _params();
        p.token = address(bad);
        uint256 id = _create(p);
        _join(id, 5);
        vm.warp(p.joinBy + 1);
        escrow.finalize(id);

        bad.arm(address(escrow), abi.encodeCall(escrow.claim, (id)));
        vm.prank(players[0]);
        vm.expectRevert(ReentrancyGuard.ReentrancyGuardReentrantCall.selector);
        escrow.claim(id);
    }

    function _slice(bytes memory data, uint256 len) internal pure returns (bytes memory out) {
        out = new bytes(len);
        for (uint256 i; i < len; i++) {
            out[i] = data[i];
        }
    }

    /// Flips (s, v) to the high-s twin, which ecrecover accepts but OZ ECDSA rejects.
    function _malleate(bytes memory sig) internal pure returns (bytes memory) {
        bytes32 r;
        bytes32 s;
        uint8 v;
        assembly ("memory-safe") {
            r := mload(add(sig, 0x20))
            s := mload(add(sig, 0x40))
            v := byte(0, mload(add(sig, 0x60)))
        }
        uint256 n = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141;
        return abi.encodePacked(r, bytes32(n - uint256(s)), v == 27 ? uint8(28) : uint8(27));
    }
}
