package routing

import (
	"context"
	"fmt"

	"github.com/mistakeknot/intercom/internal/db"
)

// Evidence stores routing decisions in Postgres for observability.
type Evidence struct {
	pool *db.Pool
}

func NewEvidence(pool *db.Pool) *Evidence {
	return &Evidence{pool: pool}
}

// RecordSelection stores a routing decision for post-hoc analysis.
func (e *Evidence) RecordSelection(ctx context.Context, phase, model, runtime, source string) error {
	return e.pool.SetRouterState(ctx,
		fmt.Sprintf("last_route:%s", phase),
		fmt.Sprintf("%s|%s|%s", model, runtime, source),
	)
}

// GetLastSelection retrieves the last routing decision for a phase.
func (e *Evidence) GetLastSelection(ctx context.Context, phase string) (string, error) {
	return e.pool.GetRouterState(ctx, fmt.Sprintf("last_route:%s", phase))
}
