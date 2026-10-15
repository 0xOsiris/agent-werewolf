// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity ^0.8.24;

import {MerkleProof} from "@openzeppelin/contracts/utils/cryptography/MerkleProof.sol";
import {IWerewolfEscrow} from "../src/IWerewolfEscrow.sol";
import {WerewolfEscrow} from "../src/WerewolfEscrow.sol";
import {EscrowBase} from "./Base.t.sol";
import {Merkle} from "./utils/Merkle.sol";

/// Checks the contract against the Rust vectors in crates/verify/tests.
contract VectorsTest is EscrowBase {
    using Merkle for bytes32[];

    string internal vectors;
    string internal transcript;

    function setUp() public override {
        super.setUp();
        vectors = vm.readFile("../crates/verify/tests/vectors.json");
        transcript = vm.readFile("../crates/verify/tests/transcript.json");
        assertEq(block.chainid, vm.parseJsonUint(vectors, ".domain.chain_id"));

        // Receipts and public values are bound to the escrow address, so deploy at the vectors' one.
        address at = vm.parseJsonAddress(vectors, ".domain.escrow");
        deployCodeTo(
            "WerewolfEscrow.sol:WerewolfEscrow",
            abi.encode(rng, verifier, VKEY, CHALLENGE_WINDOW, PROOF_WINDOW, DISPUTE_WINDOW),
            at
        );
        escrow = WerewolfEscrow(at);
        _fund(operator);
        for (uint256 i; i < players.length; i++) {
            _fund(players[i]);
        }
    }

    function test_Vector_Domain() public view {
        assertEq(vm.parseJsonUint(vectors, ".domain.game_id"), 1);
        assertEq(
            abi.encodePacked(uint64(block.chainid), address(escrow), uint256(1)),
            vm.parseJsonBytes(vectors, ".domain.bytes")
        );
    }

    function test_Vector_RulesSeedAndRoster() public view {
        bytes32 rulesHash = keccak256(
            abi.encode(
                uint8(vm.parseJsonUint(vectors, ".rules.rules.wolves")),
                vm.parseJsonBool(vectors, ".rules.rules.sheriff"),
                vm.parseJsonBool(vectors, ".rules.rules.cupid")
            )
        );
        assertEq(rulesHash, vm.parseJsonBytes32(vectors, ".rules.rules_hash"));
        assertEq(rulesHash, _rulesHash());

        bytes32 seed = vm.parseJsonBytes32(vectors, ".deal.seed");
        assertEq(
            keccak256(abi.encodePacked(seed)), vm.parseJsonBytes32(vectors, ".deal.seed_commit")
        );

        (address[] memory seatPlayers, bytes32[] memory keys) = _roster(vectors, ".roster.seats");
        bytes32 rosterHash = keccak256(abi.encode(seatPlayers, keys));
        assertEq(rosterHash, vm.parseJsonBytes32(vectors, ".roster.roster_hash"));
        assertEq(rosterHash, vm.parseJsonBytes32(vectors, ".deal.roster_hash"));
        assertEq(
            keccak256(
                abi.encodePacked(
                    "werewolf/deal/v1",
                    seed,
                    vm.parseJsonBytes32(vectors, ".deal.randomness"),
                    rosterHash
                )
            ),
            vm.parseJsonBytes32(vectors, ".deal.engine_seed")
        );
    }

    function test_Vector_ReceiptDigests() public view {
        bytes memory domain = vm.parseJsonBytes(vectors, ".domain.bytes");
        uint256 count;
        for (; vm.keyExistsJson(vectors, _at(".acts", count)); count++) {
            string memory act = _at(".acts", count);
            bytes memory message = vm.parseJsonBytes(vectors, string.concat(act, ".message"));
            bytes32 actHash = vm.parseJsonBytes32(vectors, string.concat(act, ".act_hash"));
            uint64 turnId = uint64(vm.parseJsonUint(vectors, string.concat(act, ".turn_id")));
            uint32 seat = uint32(vm.parseJsonUint(vectors, string.concat(act, ".seat")));

            assertEq(
                message,
                abi.encodePacked(
                    "werewolf/act/v1",
                    domain,
                    turnId,
                    uint32(vm.parseJsonUint(vectors, string.concat(act, ".step"))),
                    seat,
                    vm.parseJsonBytes32(vectors, string.concat(act, ".log_hash")),
                    vm.parseJsonString(vectors, string.concat(act, ".action_json"))
                )
            );
            assertEq(sha256(message), actHash);
            assertEq(
                escrow.receiptDigest(1, turnId, seat, actHash),
                vm.parseJsonBytes32(vectors, string.concat(act, ".receipt_digest"))
            );
        }
        assertGt(count, 0);
    }

    function test_Vector_OpLog() public {
        uint256 id = _startedGame(5);
        bytes32[] memory hashes = vm.parseJsonBytes32Array(vectors, ".op_log.hashes");
        address[] memory seatPlayers = new address[](5);
        for (uint256 i; i < 5; i++) {
            seatPlayers[i] = players[i];
        }
        _replayOpLog(id, vectors, ".op_log.entries", seatPlayers, hashes);
        assertEq(_info(id).opLogHash, hashes[hashes.length - 1]);
    }

    function test_Vector_EmptyMerkleRoot() public pure {
        assertEq(new bytes32[](0).root(), bytes32(0));
    }

    function test_Vector_MerkleProofs() public view {
        uint256 trees;
        for (; vm.keyExistsJson(vectors, _at(".merkle", trees)); trees++) {
            string memory tree = _at(".merkle", trees);
            bytes32[] memory leaves =
                vm.parseJsonBytes32Array(vectors, string.concat(tree, ".leaves"));
            bytes32 root = vm.parseJsonBytes32(vectors, string.concat(tree, ".root"));
            assertEq(leaves.root(), root);
            for (uint256 i; i < leaves.length; i++) {
                bytes32[] memory proof = leaves.length == 1
                    ? new bytes32[](0)
                    : vm.parseJsonBytes32Array(vectors, _at(string.concat(tree, ".proofs"), i));
                assertTrue(MerkleProof.verify(proof, root, leaves[i]));
                assertEq(keccak256(abi.encode(proof)), keccak256(abi.encode(leaves.proof(i))));
            }
        }
        assertGt(trees, 0);
    }

    /// Disputes the vectors' receipts and answers them against a root built like the vector trees.
    function test_Vector_AnswerDisputes() public {
        (address[] memory seatPlayers, bytes32[] memory keys) = _roster(transcript, ".seats");
        uint256 id = _vectorGame(seatPlayers, keys);

        bytes32[] memory leaves = vm.parseJsonBytes32Array(vectors, ".merkle[3].leaves");
        uint256 acts;
        for (; vm.keyExistsJson(vectors, _at(".acts", acts)); acts++) {
            leaves[acts] =
                vm.parseJsonBytes32(vectors, string.concat(_at(".acts", acts), ".receipt_digest"));
        }
        vm.prank(operator);
        escrow.propose(id, vm.parseJsonBytes32(vectors, ".deal.seed"), 1, 1, leaves.root(), "");

        for (uint256 i; i < acts; i++) {
            string memory act = _at(".acts", i);
            uint64 turnId = uint64(vm.parseJsonUint(vectors, string.concat(act, ".turn_id")));
            uint32 seat = uint32(vm.parseJsonUint(vectors, string.concat(act, ".seat")));
            bytes32 actHash = vm.parseJsonBytes32(vectors, string.concat(act, ".act_hash"));
            uint256 disputeId = _dispute(seatPlayers[seat], id, turnId, seat, actHash);
            escrow.answerDispute(id, disputeId, leaves.proof(i));
        }
        assertEq(_info(id).openDisputes, 0);
        assertEq(escrow.claimable(id, operator), acts * (STAKE / 4));
    }

    /// Replays transcript.json on-chain and proves it with the vectors' public values.
    function test_Vector_ProveTranscript() public {
        (address[] memory seatPlayers, bytes32[] memory keys) = _roster(transcript, ".seats");
        uint256 id = _vectorGame(seatPlayers, keys);
        _replayOpLog(id, transcript, ".op_log", seatPlayers, new bytes32[](0));

        uint8 winnerTeam = uint8(vm.parseJsonUint(transcript, ".result.winner_team"));
        uint256 winners = vm.parseJsonUint(transcript, ".result.winners");
        bytes32 root = vm.parseJsonBytes32(transcript, ".result.transcript_root");
        vm.prank(operator);
        escrow.propose(
            id, vm.parseJsonBytes32(transcript, ".seed"), winnerTeam, winners, root, "vectors"
        );

        bytes memory publicValues = vm.parseJsonBytes(vectors, ".transcript.public_values");
        IWerewolfEscrow.PublicValues memory pv =
            abi.decode(publicValues, (IWerewolfEscrow.PublicValues));
        IWerewolfEscrow.GameView memory g = _info(id);
        assertEq(pv.rosterHash, g.rosterHash);
        assertEq(pv.opLogHash, g.opLogHash);
        assertEq(pv.transcriptRoot, root);

        vm.expectEmit(address(escrow));
        emit IWerewolfEscrow.Proved(id, true);
        escrow.prove(id, publicValues, "");

        vm.warp(g.challengeEnds);
        escrow.finalize(id);
        assertEq(uint8(_status(id)), uint8(IWerewolfEscrow.Status.Settled));
        for (uint256 i; i < seatPlayers.length; i++) {
            uint256 expected = (winners >> i) & 1 == 1 ? uint256(STAKE) * seatPlayers.length : 0;
            assertEq(escrow.claimable(id, seatPlayers[i]), expected);
        }
    }

    /// Game 1 on the vectors' escrow with their rules, seed commit, encryption key and randomness.
    function _vectorGame(address[] memory seatPlayers, bytes32[] memory keys)
        internal
        returns (uint256 id)
    {
        IWerewolfEscrow.CreateParams memory p = _params();
        p.maxSeats = uint8(seatPlayers.length);
        p.rulesHash = vm.parseJsonBytes32(vectors, ".rules.rules_hash");
        p.seedCommit = vm.parseJsonBytes32(vectors, ".deal.seed_commit");
        p.encKey = vm.parseJsonBytes32(transcript, ".enc_key");
        id = _create(p);
        assertEq(id, 1);
        for (uint256 i; i < seatPlayers.length; i++) {
            _fund(seatPlayers[i]);
            vm.prank(seatPlayers[i]);
            escrow.join(id, keys[i]);
        }
        vm.prank(operator);
        escrow.start(id);
        rng.fulfill(escrow, id, vm.parseJsonBytes32(transcript, ".randomness"));
    }

    /// Sends every op log entry on-chain; checks each hash when `hashes` is non-empty.
    function _replayOpLog(
        uint256 id,
        string memory json,
        string memory key,
        address[] memory seatPlayers,
        bytes32[] memory hashes
    ) internal {
        for (uint256 i; vm.keyExistsJson(json, _at(key, i)); i++) {
            string memory entry = _at(key, i);
            uint32 round = uint32(vm.parseJsonUint(json, string.concat(entry, ".round")));
            if (
                keccak256(bytes(vm.parseJsonString(json, string.concat(entry, ".kind"))))
                    == keccak256("force")
            ) {
                uint256 seat = vm.parseJsonUint(json, string.concat(entry, ".seat"));
                vm.prank(seatPlayers[seat]);
                escrow.forceAction(
                    id, round, vm.parseJsonBytes(json, string.concat(entry, ".payload"))
                );
            } else {
                vm.prank(operator);
                escrow.checkpoint(id, round);
            }
            if (hashes.length > 0) assertEq(_info(id).opLogHash, hashes[i]);
        }
    }

    function _roster(string memory json, string memory key)
        internal
        view
        returns (address[] memory seatPlayers, bytes32[] memory keys)
    {
        uint256 count;
        while (vm.keyExistsJson(json, _at(key, count))) {
            count++;
        }
        seatPlayers = new address[](count);
        keys = new bytes32[](count);
        for (uint256 i; i < count; i++) {
            seatPlayers[i] = vm.parseJsonAddress(json, string.concat(_at(key, i), ".address"));
            keys[i] = vm.parseJsonBytes32(json, string.concat(_at(key, i), ".session_key"));
        }
    }

    function _at(string memory key, uint256 index) internal pure returns (string memory) {
        return string.concat(key, "[", vm.toString(index), "]");
    }
}
