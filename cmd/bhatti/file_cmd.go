package main

import (
	"bytes"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"

	"github.com/spf13/cobra"
)

var fileCmd = &cobra.Command{
	Use:   "file <read|write|ls> <id|name> <path>",
	Short: "Read, write, and list files in a sandbox",
	Example: `  bhatti file read dev /workspace/app.js
  echo 'hello' | bhatti file write dev /workspace/greeting.txt
  bhatti file ls dev /workspace/`,
}

var fileReadCmd = &cobra.Command{
	Use:               "read <id|name> <path>",
	Short:             "Read a file from a sandbox",
	Args:              cobra.ExactArgs(2),
	ValidArgsFunction: completeSandboxNames,
	RunE: func(cmd *cobra.Command, args []string) error {
		setupTiming(cmd)
		defer printTiming()

		id, err := resolveID(args[0])
		if err != nil {
			return err
		}
		resp, err := apiRequest("GET",
			"/sandboxes/"+id+"/files?path="+url.QueryEscape(args[1]), nil)
		if err != nil {
			return err
		}
		defer resp.Body.Close()
		if resp.StatusCode >= 400 {
			body, _ := io.ReadAll(resp.Body)
			return fmt.Errorf("%s", body)
		}
		io.Copy(os.Stdout, resp.Body)
		return nil
	},
}

var fileWriteCmd = &cobra.Command{
	Use:               "write <id|name> <path>",
	Short:             "Write a file to a sandbox (reads from stdin)",
	Args:              cobra.ExactArgs(2),
	ValidArgsFunction: completeSandboxNames,
	RunE: func(cmd *cobra.Command, args []string) error {
		setupTiming(cmd)
		defer printTiming()

		id, err := resolveID(args[0])
		if err != nil {
			return err
		}
		// Read all stdin to get Content-Length
		data, _ := io.ReadAll(os.Stdin)
		req, _ := http.NewRequest("PUT",
			apiURL+"/sandboxes/"+id+"/files?path="+url.QueryEscape(args[1]),
			bytes.NewReader(data))
		req.Header.Set("Content-Type", "application/octet-stream")
		req.ContentLength = int64(len(data))
		if apiToken != "" {
			req.Header.Set("Authorization", "Bearer "+apiToken)
		}
		resp, err := httpClient().Do(req)
		if err != nil {
			return err
		}
		defer resp.Body.Close()
		if resp.StatusCode >= 400 {
			body, _ := io.ReadAll(resp.Body)
			return fmt.Errorf("%s", body)
		}
		fmt.Println("ok")
		return nil
	},
}

var fileLSCmd = &cobra.Command{
	Use:               "ls <id|name> <path>",
	Short:             "List files in a sandbox directory",
	Args:              cobra.ExactArgs(2),
	ValidArgsFunction: completeSandboxNames,
	RunE: func(cmd *cobra.Command, args []string) error {
		setupTiming(cmd)
		defer printTiming()

		id, err := resolveID(args[0])
		if err != nil {
			return err
		}
		var files []struct {
			Name  string `json:"name"`
			Size  int64  `json:"size"`
			IsDir bool   `json:"is_dir"`
			Mode  string `json:"mode"`
		}
		if err := apiJSON("GET",
			"/sandboxes/"+id+"/files?path="+url.QueryEscape(args[1])+"&ls=true",
			nil, &files); err != nil {
			return err
		}
		if isJSON(cmd) {
			outputJSON(files)
		} else {
			for _, f := range files {
				dirFlag := "-"
				if f.IsDir {
					dirFlag = "d"
				}
				fmt.Printf("%s%s %8d %s\n", dirFlag, f.Mode, f.Size, f.Name)
			}
		}
		return nil
	},
}

func init() {
	fileCmd.AddCommand(fileReadCmd)
	fileCmd.AddCommand(fileWriteCmd)
	fileCmd.AddCommand(fileLSCmd)
}
