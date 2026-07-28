package interopsmoke

import (
	"context"
	"errors"
	"testing"

	"github.com/stretchr/testify/require"

	"github.com/ethereum-optimism/optimism/op-service/apis"
	"github.com/ethereum-optimism/optimism/op-service/eth"
)

func TestValidateInvalidMessageOptions(t *testing.T) {
	for _, tc := range []struct {
		name               string
		blocks, txPerBlock uint
		wantErr            bool
	}{
		{"valid", 2, 3, false},
		{"zero blocks", 0, 1, true},
		{"zero tx per block", 1, 0, true},
	} {
		t.Run(tc.name, func(t *testing.T) {
			if err := validateInvalidMessageOptions(tc.blocks, tc.txPerBlock); (err != nil) != tc.wantErr {
				t.Fatalf("error = %v, wantErr %v", err, tc.wantErr)
			}
		})
	}
}

// stubEthClient answers BlockRefByLabel from refs. Every other method panics on a nil interface,
// which keeps the stub honest: waitForHead must not reach for anything else.
type stubEthClient struct {
	apis.EthClient
	refs func() (eth.BlockRef, error)
}

func (s *stubEthClient) BlockRefByLabel(context.Context, eth.BlockLabel) (eth.BlockRef, error) {
	return s.refs()
}

func TestWaitForHead(t *testing.T) {
	atTime := func(minTime uint64) func(eth.BlockRef) bool {
		return func(head eth.BlockRef) bool { return head.Time >= minTime }
	}

	t.Run("retries a failed head lookup", func(t *testing.T) {
		calls := 0
		chain := &remoteChain{name: "chainA", ethClient: &stubEthClient{refs: func() (eth.BlockRef, error) {
			calls++
			if calls == 1 {
				return eth.BlockRef{}, errors.New("transient rpc failure")
			}
			return eth.BlockRef{Number: 2, Time: 20}, nil
		}}}

		require.NoError(t, waitForHead(context.Background(), chain, "timestamp >= 20", atTime(20)))
		require.Equal(t, 2, calls, "should have retried past the failure")
	})

	t.Run("returns the context error", func(t *testing.T) {
		ctx, cancel := context.WithCancel(context.Background())
		chain := &remoteChain{name: "chainA", ethClient: &stubEthClient{refs: func() (eth.BlockRef, error) {
			cancel()
			return eth.BlockRef{}, errors.New("transient rpc failure")
		}}}

		require.ErrorIs(t, waitForHead(ctx, chain, "timestamp >= 20", atTime(20)), context.Canceled)
	})
}
