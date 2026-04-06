package store

import (
	"database/sql"
	"fmt"
	"strings"
	"time"
)

type PublishRule struct {
	ID        string    `json:"id"`
	SandboxID string    `json:"sandbox_id"`
	UserID    string    `json:"user_id"`
	Port      int       `json:"port"`
	Alias     string    `json:"alias"`
	CreatedAt time.Time `json:"created_at"`
}

// CreatePublishRule inserts a new publish rule.
// Returns a descriptive error on UNIQUE constraint violations.
func (s *Store) CreatePublishRule(rule PublishRule) error {
	_, err := s.db.Exec(
		`INSERT INTO publish_rules (id, sandbox_id, user_id, port, alias)
		 VALUES (?, ?, ?, ?, ?)`,
		rule.ID, rule.SandboxID, rule.UserID, rule.Port, rule.Alias,
	)
	if err != nil {
		if strings.Contains(err.Error(), "UNIQUE constraint") {
			if strings.Contains(err.Error(), "alias") {
				return fmt.Errorf("alias %q is already taken", rule.Alias)
			}
			return fmt.Errorf("port %d is already published for this sandbox", rule.Port)
		}
		return err
	}
	return nil
}

// GetPublishRuleByAlias looks up a publish rule by its public alias.
func (s *Store) GetPublishRuleByAlias(alias string) (*PublishRule, error) {
	var r PublishRule
	err := s.db.QueryRow(
		`SELECT id, sandbox_id, user_id, port, alias, created_at
		 FROM publish_rules WHERE alias = ?`, alias,
	).Scan(&r.ID, &r.SandboxID, &r.UserID, &r.Port, &r.Alias, &r.CreatedAt)
	if err == sql.ErrNoRows {
		return nil, fmt.Errorf("no publish rule for alias %q", alias)
	}
	return &r, err
}

// ListPublishRules returns all publish rules for a sandbox.
func (s *Store) ListPublishRules(sandboxID string) ([]PublishRule, error) {
	rows, err := s.db.Query(
		`SELECT id, sandbox_id, user_id, port, alias, created_at
		 FROM publish_rules WHERE sandbox_id = ?`, sandboxID,
	)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var rules []PublishRule
	for rows.Next() {
		var r PublishRule
		if err := rows.Scan(&r.ID, &r.SandboxID, &r.UserID, &r.Port, &r.Alias, &r.CreatedAt); err != nil {
			return nil, err
		}
		rules = append(rules, r)
	}
	return rules, rows.Err()
}

// ListUserPublishRules returns all publish rules for sandboxes owned by a user.
func (s *Store) ListUserPublishRules(userID string) ([]PublishRule, error) {
	rows, err := s.db.Query(
		`SELECT id, sandbox_id, user_id, port, alias, created_at
		 FROM publish_rules WHERE user_id = ?`, userID)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var rules []PublishRule
	for rows.Next() {
		var r PublishRule
		if err := rows.Scan(&r.ID, &r.SandboxID, &r.UserID, &r.Port, &r.Alias, &r.CreatedAt); err != nil {
			return nil, err
		}
		rules = append(rules, r)
	}
	return rules, rows.Err()
}

// DeletePublishRule removes a publish rule scoped to user + sandbox + port.
func (s *Store) DeletePublishRule(userID, sandboxID string, port int) error {
	res, err := s.db.Exec(
		`DELETE FROM publish_rules WHERE user_id = ? AND sandbox_id = ? AND port = ?`,
		userID, sandboxID, port,
	)
	if err != nil {
		return err
	}
	if n, _ := res.RowsAffected(); n == 0 {
		return fmt.Errorf("no publish rule for port %d on this sandbox", port)
	}
	return nil
}

// DeletePublishRulesForSandbox removes all publish rules for a sandbox.
func (s *Store) DeletePublishRulesForSandbox(sandboxID string) (int64, error) {
	res, err := s.db.Exec(`DELETE FROM publish_rules WHERE sandbox_id = ?`, sandboxID)
	if err != nil {
		return 0, err
	}
	return res.RowsAffected()
}

// CleanupOrphanedPublishRules removes rules for destroyed/unknown/missing sandboxes.
func (s *Store) CleanupOrphanedPublishRules() (int64, error) {
	res, err := s.db.Exec(`
		DELETE FROM publish_rules WHERE sandbox_id NOT IN (
			SELECT id FROM sandboxes WHERE status NOT IN ('destroyed', 'unknown')
		)`)
	if err != nil {
		return 0, err
	}
	return res.RowsAffected()
}

// --- Volume Backups ---

