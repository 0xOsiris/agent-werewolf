// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity ^0.8.24;

import {IWerewolfEscrow} from "../src/IWerewolfEscrow.sol";
import {EscrowBase} from "./Base.t.sol";
import {Merkle} from "./utils/Merkle.sol";

/// Every end state pays out exactly what was deposited, with no dust left behind.
contract PayoutsTest is EscrowBase {
    using Merkle for bytes32[];

    enum End {
        Cancelled,
        VoidAtPlayBy,
        Settled,
        SettledMatchingProof,
        SettledCorrectingProof,
        ChallengeMatched,
        ChallengeOverturned,
        ChallengeUnproven,
        DisputeAnswered,
        DisputeUnanswered
    }

    struct Run {
        uint256 id;
        uint256 seatCount;
        uint256 winners;
        uint256 proved;
        bytes32[] leaves;
        address challenger;
        address disputer;
    }

    function testFuzz_PayoutsConserveDeposits(
        uint256 seatSeed,
        uint256 maxSeed,
        uint256 stakeSeed,
        uint256 winnersSeed,
        uint256 provedSeed,
        uint256 endSeed,
        bool operatorPlays,
        bool seatsAct
    ) public {
        Run memory run;
        run.seatCount = bound(seatSeed, 5, 12);
        run.winners = bound(winnersSeed, 1, (1 << run.seatCount) - 1);
        run.proved = bound(provedSeed, 1, (1 << run.seatCount) - 1);
        run.challenger = seatsAct ? players[1] : outsider;
        run.disputer = seatsAct ? players[2] : outsider;
        End end = End(bound(endSeed, 0, uint256(type(End).max)));

        IWerewolfEscrow.CreateParams memory p = _params();
        p.maxSeats = uint8(bound(maxSeed, run.seatCount, 12));
        p.stake = uint96(bound(stakeSeed, 1, 1e24));
        run.id = _create(p);
        for (uint256 i; i < run.seatCount; i++) {
            vm.prank(operatorPlays && i == 0 ? operator : players[i]);
            escrow.join(run.id, _sessionKey(i));
        }

        if (end == End.Cancelled) {
            vm.warp(p.joinBy + 1);
        } else {
            _start(run.id);
            if (end == End.VoidAtPlayBy) vm.warp(p.playBy + 1);
            else _settle(run, end);
        }

        uint256 deposited = token.balanceOf(address(escrow));
        escrow.finalize(run.id);
        assertEq(uint8(_status(run.id)), uint8(_expectedStatus(end)));
        assertEq(_claimAll(run.id, _everyone()), deposited);
        assertEq(token.balanceOf(address(escrow)), 0);
    }

    function _expectedStatus(End end) internal pure returns (IWerewolfEscrow.Status) {
        if (end == End.Cancelled) return IWerewolfEscrow.Status.Cancelled;
        if (end == End.VoidAtPlayBy || end == End.ChallengeUnproven || end == End.DisputeUnanswered)
        {
            return IWerewolfEscrow.Status.Void;
        }
        return IWerewolfEscrow.Status.Settled;
    }

    function _settle(Run memory run, End end) internal {
        run.leaves = new bytes32[](run.seatCount);
        for (uint256 i; i < run.seatCount; i++) {
            run.leaves[i] = escrow.receiptDigest(run.id, uint64(i), uint32(i), bytes32(i));
        }
        bytes32 root = run.leaves.root();
        _propose(run.id, run.winners, root);
        uint64 challengeEnds = _info(run.id).challengeEnds;

        if (end == End.Settled) {
            vm.warp(challengeEnds);
        } else if (end == End.SettledMatchingProof) {
            _prove(run.id, 1, run.winners, root);
            vm.warp(challengeEnds);
        } else if (end == End.SettledCorrectingProof) {
            _prove(run.id, 0, run.proved, OTHER_ROOT);
            vm.warp(challengeEnds);
        } else if (end == End.DisputeAnswered) {
            uint256 d = _dispute(run.disputer, run.id, 0, 0, bytes32(0));
            escrow.answerDispute(run.id, d, run.leaves.proof(0));
            vm.warp(challengeEnds);
        } else if (end == End.DisputeUnanswered) {
            uint256 d = _dispute(run.disputer, run.id, 1, 1, bytes32(uint256(1)));
            _dispute(players[3], run.id, 99, 0, keccak256("dropped"));
            escrow.answerDispute(run.id, d, run.leaves.proof(1));
            vm.warp(block.timestamp + DISPUTE_WINDOW + 1);
        } else {
            vm.prank(run.challenger);
            escrow.challenge(run.id);
            uint64 proofDeadline = _info(run.id).proofDeadline;
            if (end == End.ChallengeMatched) _prove(run.id, 1, run.winners, root);
            if (end == End.ChallengeOverturned) _prove(run.id, 2, run.proved, OTHER_ROOT);
            vm.warp(end == End.ChallengeUnproven ? proofDeadline + 1 : proofDeadline);
        }
    }
}
