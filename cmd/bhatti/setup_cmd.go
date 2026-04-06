package main

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/sahil-shubham/bhatti/pkg"
	"github.com/spf13/cobra"
	"golang.org/x/term"
)


// --- setup ---

var setupCmd = &cobra.Command{
	Use:   "setup",
	Short: "Configure CLI endpoint and API key",
	Long: `Interactive setup for remote CLI users. Prompts for the API endpoint
and API key, saves to ~/.bhatti/config.yaml, and tests the connection.`,
	Example: `  bhatti setup`,
	RunE: func(cmd *cobra.Command, args []string) error {
		fmt.Printf("API endpoint [%s]: ", apiURL)
		var endpoint string
		fmt.Scanln(&endpoint)
		if endpoint == "" {
			endpoint = apiURL
		}

		fmt.Print("API key: ")
		keyBytes, err := term.ReadPassword(int(os.Stdin.Fd()))
		fmt.Println()
		if err != nil {
			return fmt.Errorf("read key: %w", err)
		}
		key := strings.TrimSpace(string(keyBytes))
		if key == "" {
			return fmt.Errorf("API key is required")
		}

		// Write config
		cfgDir := pkg.DefaultDataDir()
		os.MkdirAll(cfgDir, 0700)
		cfgPath := filepath.Join(cfgDir, "config.yaml")

		var cfgContent string
		if strings.HasPrefix(endpoint, "https://") || strings.HasPrefix(endpoint, "http://") {
			// Remote endpoint — save URL and token
			cfgContent = fmt.Sprintf("api_url: %s\nauth_token: %s\n", endpoint, key)
		} else {
			// Local — save listen address and token
			cfgContent = fmt.Sprintf("listen: %s\nauth_token: %s\n", endpoint, key)
		}

		if err := os.WriteFile(cfgPath, []byte(cfgContent), 0600); err != nil {
			return fmt.Errorf("write config: %w", err)
		}
		fmt.Printf("Saved to %s\n", cfgPath)

		// Test connection using an authenticated endpoint.
		// /health is unauthenticated — it only proves the server is
		// reachable, not that the API key is valid. Use /sandboxes
		// (GET) which requires auth, so we catch bad keys immediately.
		fmt.Print("Testing connection... ")
		apiURL = endpoint
		apiToken = key
		var sandboxes []any
		if err := apiJSON("GET", "/sandboxes", nil, &sandboxes); err != nil {
			fmt.Printf("✗ %v\n", err)
			return nil
		}
		fmt.Printf("✓ authenticated (%d sandboxes)\n", len(sandboxes))

		// Suggest shell completions
		shell := os.Getenv("SHELL")
		switch {
		case strings.HasSuffix(shell, "/zsh"):
			fmt.Println("\nEnable completions:")
			fmt.Println("  echo 'source <(bhatti completion zsh)' >> ~/.zshrc")
		case strings.HasSuffix(shell, "/bash"):
			fmt.Println("\nEnable completions:")
			fmt.Println("  echo 'source <(bhatti completion bash)' >> ~/.bashrc")
		case strings.HasSuffix(shell, "/fish"):
			fmt.Println("\nEnable completions:")
			fmt.Println("  bhatti completion fish > ~/.config/fish/completions/bhatti.fish")
		}
		return nil
	},
}
