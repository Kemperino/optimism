package sdm

import (
	"encoding/json"
	"strings"

	"github.com/ethereum-optimism/optimism/op-acceptance-tests/tests/sdm/sdmtest"
	sdmpkg "github.com/ethereum-optimism/optimism/op-chain-ops/pkg/sdm"
	"github.com/ethereum-optimism/optimism/op-devstack/devtest"
	"github.com/ethereum-optimism/optimism/op-devstack/dsl"
	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/common/hexutil"
	gethtypes "github.com/ethereum/go-ethereum/core/types"
)

// getOPGasRefund reads the opGasRefund field from a transaction receipt via
// raw JSON RPC. The boolean return value reports whether the field was present.
func getOPGasRefund(t devtest.T, l2EL *dsl.L2ELNode, txHash common.Hash) (uint64, bool) {
	rpcClient := l2EL.Escape().L2EthClient().RPC()
	var raw json.RawMessage
	err := rpcClient.CallContext(t.Ctx(), &raw, "eth_getTransactionReceipt", txHash)
	t.Require().NoError(err, "eth_getTransactionReceipt RPC failed for tx %s", txHash)
	t.Require().NotNil(raw, "receipt %s not found", txHash)

	var result struct {
		OPGasRefund *hexutil.Uint64 `json:"opGasRefund"`
	}
	err = json.Unmarshal(raw, &result)
	t.Require().NoError(err, "failed to unmarshal receipt %s", txHash)
	if result.OPGasRefund == nil {
		return 0, false
	}
	return uint64(*result.OPGasRefund), true
}

func assertFixtureBlockOracle(t devtest.T, sys *sdmtest.RethSystem, block *sdmpkg.RPCBlock, blockNum uint64) {
	postExecTx, postExecPos := sdmpkg.FindPostExecTransaction(block)
	t.Require().NotNil(postExecTx, "fixture block must contain a post-exec tx")
	t.Require().Equal(len(block.Transactions)-1, postExecPos, "fixture post-exec tx must be trailing")

	postExecCount := 0
	expectedIndexes := make([]uint64, 0, len(block.Transactions))
	for i, tx := range block.Transactions {
		switch uint64(tx.Type) {
		case uint64(sdmpkg.SDMTxType):
			postExecCount++
		case uint64(gethtypes.DepositTxType):
		default:
			expectedIndexes = append(expectedIndexes, uint64(i))
		}
	}
	t.Require().Equal(1, postExecCount, "fixture block must contain exactly one post-exec tx")
	assertPostExecTxHashIsCanonical(t, sys.L2EL, postExecTx)

	payload, err := sdmpkg.DecodePayload(postExecTx.Input)
	t.Require().NoError(err, "fixture post-exec payload must decode")
	t.Require().Equal(blockNum, payload.BlockNumber, "fixture payload must anchor to its block")
	t.Require().Equal(sdmpkg.PostExecPayloadVersion, payload.Version, "fixture payload version must match")
	t.Require().Len(payload.GasRefundEntries, len(expectedIndexes),
		"fixture must emit one entry per committed normal transaction")

	for i, entry := range payload.GasRefundEntries {
		t.Require().Equal(expectedIndexes[i], entry.Index, "fixture entry indexes must match normal tx order")
		t.Require().Equal(uint64(1), entry.GasRefund, "fixture refund must be exactly one gas")
		target := block.Transactions[entry.Index]
		refund, present := getOPGasRefund(t, sys.L2EL, target.Hash)
		t.Require().True(present, "fixture target receipt %s must expose opGasRefund", target.Hash)
		t.Require().Equal(uint64(1), refund, "fixture target receipt must expose one gas")
	}

	for _, tx := range block.Transactions {
		if uint64(tx.Type) != uint64(gethtypes.DepositTxType) && uint64(tx.Type) != uint64(sdmpkg.SDMTxType) {
			continue
		}
		refund, present := getOPGasRefund(t, sys.L2EL, tx.Hash)
		t.Require().False(present, "deposit and post-exec receipts must omit opGasRefund")
		t.Require().Zero(refund, "non-target receipt refund must decode to zero")
	}
}

func assertFixtureVerifierReceipts(t devtest.T, sys *sdmtest.RethSystem, block *sdmpkg.RPCBlock) {
	verifierBlock := sdmtest.GetBlockWithTxs(t, sys.L2ELVerifier, uint64(block.Number))
	t.Require().Equal(block.Hash, verifierBlock.Hash, "stock verifier fixture block hash must match")
	t.Require().Len(verifierBlock.Transactions, len(block.Transactions),
		"stock verifier fixture transaction count must match")
	for i, tx := range block.Transactions {
		t.Require().Equal(tx.Hash, verifierBlock.Transactions[i].Hash,
			"stock verifier transaction hash at index %d must match", i)
		producerRefund, producerPresent := getOPGasRefund(t, sys.L2EL, tx.Hash)
		verifierRefund, verifierPresent := getOPGasRefund(t, sys.L2ELVerifier, tx.Hash)
		t.Require().Equal(producerPresent, verifierPresent,
			"stock verifier opGasRefund field presence for tx %s must match", tx.Hash)
		t.Require().Equal(producerRefund, verifierRefund,
			"stock verifier opGasRefund for tx %s must match", tx.Hash)
	}
}

// assertPostExecTxHashIsCanonical asserts op-reth serves and resolves the post-exec tx (and its
// receipt) under the hash Go's PostExecTx.Hash() produces — keccak256(0x7D || Data). Hashing via
// Hash() rather than a hand-rolled keccak checks the Go hasher and op-reth agree — the cross-client
// guarantee op-service/sources relies on.
func assertPostExecTxHashIsCanonical(t devtest.T, l2EL *dsl.L2ELNode, postExecTx *sdmpkg.RPCTransaction) {
	wantHash := gethtypes.NewTx(&gethtypes.PostExecTx{Data: []byte(postExecTx.Input)}).Hash()

	t.Require().Equal(wantHash, postExecTx.Hash,
		"op-reth-served post-exec tx hash %s must equal PostExecTx.Hash() %s (keccak256(0x7D || Data), matching TxDeposit)",
		postExecTx.Hash, wantHash)

	rpcClient := l2EL.Escape().L2EthClient().RPC()

	var txRaw json.RawMessage
	err := rpcClient.CallContext(t.Ctx(), &txRaw, "eth_getTransactionByHash", wantHash)
	t.Require().NoError(err, "eth_getTransactionByHash RPC failed for canonical hash %s", wantHash)
	t.Require().False(isNullJSONResult(txRaw),
		"op-reth must resolve the post-exec tx under its canonical hash %s", wantHash)

	var receiptRaw json.RawMessage
	err = rpcClient.CallContext(t.Ctx(), &receiptRaw, "eth_getTransactionReceipt", wantHash)
	t.Require().NoError(err, "eth_getTransactionReceipt RPC failed for canonical hash %s", wantHash)
	t.Require().False(isNullJSONResult(receiptRaw),
		"op-reth must resolve the post-exec receipt under its canonical hash %s", wantHash)
}

// isNullJSONResult reports whether a raw JSON-RPC result is absent (null or empty) — i.e. not found.
func isNullJSONResult(raw json.RawMessage) bool {
	return len(raw) == 0 || strings.TrimSpace(string(raw)) == "null"
}
