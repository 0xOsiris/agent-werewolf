// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";
import {IWerewolfEscrow} from "../src/IWerewolfEscrow.sol";
import {IWerewolfErrors} from "../src/IWerewolfErrors.sol";
import {WerewolfEscrow} from "../src/WerewolfEscrow.sol";
import {SP1MockVerifier} from "../src/sp1/SP1MockVerifier.sol";
import {MockERC20} from "./mocks/MockERC20.sol";
import {MockRandomness} from "./mocks/MockRandomness.sol";

abstract contract EscrowBase is Test {
    uint64 internal constant CHALLENGE_WINDOW = 120;
    uint64 internal constant PROOF_WINDOW = 600;
    uint64 internal constant DISPUTE_WINDOW = 300;
    uint96 internal constant STAKE = 10 ether;
    uint8 internal constant MAX_SEATS = 8;
    bytes32 internal constant VKEY = keccak256("werewolf/vkey");
    bytes32 internal constant SEED = keccak256("seed");
    bytes32 internal constant RANDOMNESS = keccak256("randomness");
    bytes32 internal constant ENC_KEY = keccak256("enc-key");
    bytes32 internal constant ROOT = keccak256("root");
    bytes32 internal constant OTHER_ROOT = keccak256("other-root");

    WerewolfEscrow internal escrow;
    MockERC20 internal token;
    MockRandomness internal rng;
    SP1MockVerifier internal verifier;
    address internal operator;
    uint256 internal operatorKey;
    address internal outsider;
    address[] internal players;

    function setUp() public virtual {
        vm.warp(1_000_000);
        token = new MockERC20();
        rng = new MockRandomness();
        verifier = new SP1MockVerifier();
        escrow =
            new WerewolfEscrow(rng, verifier, VKEY, CHALLENGE_WINDOW, PROOF_WINDOW, DISPUTE_WINDOW);
        (operator, operatorKey) = makeAddrAndKey("operator");
        outsider = makeAddr("outsider");
        _fund(operator);
        _fund(outsider);
        for (uint256 i; i < 12; i++) {
            players.push(makeAddr(string.concat("player", vm.toString(i))));
            _fund(players[i]);
        }
    }

    function _fund(address account) internal {
        token.mint(account, 1e30);
        vm.prank(account);
        token.approve(address(escrow), type(uint256).max);
    }

    function _rulesHash() internal pure returns (bytes32) {
        return keccak256(abi.encode(uint8(2), true, true));
    }

    function _params() internal view returns (IWerewolfEscrow.CreateParams memory) {
        return IWerewolfEscrow.CreateParams({
            token: address(token),
            stake: STAKE,
            minSeats: 5,
            maxSeats: MAX_SEATS,
            joinBy: uint64(block.timestamp + 1 hours),
            playBy: uint64(block.timestamp + 2 hours),
            rulesHash: _rulesHash(),
            seedCommit: keccak256(abi.encodePacked(SEED)),
            encKey: ENC_KEY
        });
    }

    function _create(IWerewolfEscrow.CreateParams memory params) internal returns (uint256 id) {
        vm.prank(operator);
        id = escrow.create(params);
    }

    function _create() internal returns (uint256) {
        return _create(_params());
    }

    function _sessionKey(uint256 seat) internal pure returns (bytes32) {
        return keccak256(abi.encode("session", seat));
    }

    function _join(uint256 id, uint256 count) internal {
        for (uint256 i; i < count; i++) {
            vm.prank(players[i]);
            escrow.join(id, _sessionKey(i));
        }
    }

    function _start(uint256 id) internal {
        vm.prank(operator);
        escrow.start(id);
        rng.fulfill(escrow, id, RANDOMNESS);
    }

    function _startedGame(uint256 seatCount) internal returns (uint256 id) {
        id = _create();
        _join(id, seatCount);
        _start(id);
    }

    function _propose(uint256 id, uint256 winners, bytes32 root) internal {
        vm.prank(operator);
        escrow.propose(id, SEED, 1, winners, root, "ipfs://transcript");
    }

    function _proposedGame(uint256 seatCount, uint256 winners) internal returns (uint256 id) {
        id = _startedGame(seatCount);
        _propose(id, winners, ROOT);
    }

    function _info(uint256 id) internal view returns (IWerewolfEscrow.GameView memory) {
        return escrow.gameInfo(id);
    }

    function _status(uint256 id) internal view returns (IWerewolfEscrow.Status) {
        return escrow.gameInfo(id).status;
    }

    function _publicValues(uint256 id, uint8 winnerTeam, uint256 winners, bytes32 root)
        internal
        view
        returns (IWerewolfEscrow.PublicValues memory)
    {
        IWerewolfEscrow.GameView memory g = escrow.gameInfo(id);
        return IWerewolfEscrow.PublicValues({
            chainId: uint64(block.chainid),
            escrow: address(escrow),
            gameId: id,
            rulesHash: g.rulesHash,
            rosterHash: g.rosterHash,
            seedCommit: g.seedCommit,
            randomness: g.randomness,
            encKey: g.encKey,
            opLogHash: g.opLogHash,
            winnerTeam: winnerTeam,
            winners: winners,
            transcriptRoot: root
        });
    }

    function _prove(uint256 id, uint8 winnerTeam, uint256 winners, bytes32 root) internal {
        escrow.prove(id, abi.encode(_publicValues(id, winnerTeam, winners, root)), "");
    }

    function _sign(uint256 key, bytes32 digest) internal pure returns (bytes memory) {
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(key, digest);
        return abi.encodePacked(r, s, v);
    }

    function _receipt(uint256 id, uint64 turnId, uint32 seat, bytes32 actHash)
        internal
        view
        returns (bytes32 digest, bytes memory signature)
    {
        digest = escrow.receiptDigest(id, turnId, seat, actHash);
        signature = _sign(operatorKey, digest);
    }

    function _dispute(address disputer, uint256 id, uint64 turnId, uint32 seat, bytes32 actHash)
        internal
        returns (uint256 disputeId)
    {
        (, bytes memory signature) = _receipt(id, turnId, seat, actHash);
        vm.prank(disputer);
        disputeId = escrow.disputeReceipt(id, turnId, seat, actHash, signature);
    }

    /// Claims for every listed account that is owed something; returns the total paid.
    function _claimAll(uint256 id, address[] memory accounts) internal returns (uint256 total) {
        for (uint256 i; i < accounts.length; i++) {
            if (escrow.claimable(id, accounts[i]) == 0) continue;
            vm.prank(accounts[i]);
            total += escrow.claim(id);
        }
    }

    function _everyone() internal view returns (address[] memory accounts) {
        accounts = new address[](players.length + 2);
        for (uint256 i; i < players.length; i++) {
            accounts[i] = players[i];
        }
        accounts[players.length] = operator;
        accounts[players.length + 1] = outsider;
    }

    function _wrongStatus(IWerewolfEscrow.Status status) internal pure returns (bytes memory) {
        return abi.encodeWithSelector(IWerewolfErrors.WrongStatus.selector, status);
    }
}
