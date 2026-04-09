package main

import (
	"fmt"
	"os"

	"github.com/spf13/cobra"
)

var shareCmd = &cobra.Command{
	Use:   "share <sandbox>",
	Short: "Generate a web shell URL for a sandbox",
	Long: `Generate a shareable URL that opens an interactive terminal in the browser.
Each call generates a fresh token (previous token is immediately invalidated).
Use --revoke to disable shell access.`,
	Example: `  bhatti share dev
  bhatti share dev --json
  bhatti share dev --revoke`,
	Args:              cobra.ExactArgs(1),
	ValidArgsFunction: completeSandboxNames,
	Run: func(cmd *cobra.Command, args []string) {
		revoke, _ := cmd.Flags().GetBool("revoke")

		id, err := resolveID(args[0])
		if err != nil {
			fmt.Fprintf(os.Stderr, "Error: %v\n", err)
			os.Exit(1)
		}

		if revoke {
			resp, err := apiRequest("DELETE", fmt.Sprintf("/sandboxes/%s/shell-token", id), nil)
			if err != nil {
				fmt.Fprintf(os.Stderr, "Error: %v\n", err)
				os.Exit(1)
			}
			resp.Body.Close()
			if resp.StatusCode >= 400 {
				fmt.Fprintf(os.Stderr, "Error: %s\n", resp.Status)
				os.Exit(1)
			}
			if !isJSON(cmd) {
				fmt.Println("Shell access revoked.")
			}
			return
		}

		var result map[string]interface{}
		if err := apiJSON("POST", fmt.Sprintf("/sandboxes/%s/shell-token", id), nil, &result); err != nil {
			fmt.Fprintf(os.Stderr, "Error: %v\n", err)
			os.Exit(1)
		}
		if isJSON(cmd) {
			outputJSON(result)
		} else {
			fmt.Printf("Shell: %v\n", result["url"])
		}
	},
}
