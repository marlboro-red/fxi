// Benchmark adapter around unchanged Zoekt search APIs. Build from a pinned
// Zoekt checkout: go build -o /tmp/zoekt-path-server /path/to/this/main.go
// Uses the same length-prefixed JSON transport and path-only output as FXI.
package main

import (
	"context"
	"encoding/binary"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"log"
	"net"
	"strconv"
	"time"

	"github.com/sourcegraph/zoekt"
	"github.com/sourcegraph/zoekt/query"
	"github.com/sourcegraph/zoekt/search"
)

type request struct {
	Pattern string `json:"pattern"`
}
type response struct {
	Type     string   `json:"type"`
	Paths    []string `json:"file_paths"`
	Duration float64  `json:"duration_ms"`
	Error    string   `json:"message,omitempty"`
}

func serve(connection net.Conn, index zoekt.Searcher) {
	defer connection.Close()
	connection.SetDeadline(time.Now().Add(120 * time.Second))
	var length uint32
	if err := binary.Read(connection, binary.LittleEndian, &length); err != nil {
		return
	}
	if length > 1024*1024 {
		return
	}
	payload := make([]byte, length)
	if _, err := io.ReadFull(connection, payload); err != nil {
		return
	}
	var req request
	if err := json.Unmarshal(payload, &req); err != nil {
		return
	}
	started := time.Now()
	reply := response{Type: "ContentSearch", Paths: []string{}}
	q, err := query.Parse("case:yes type:file content:" + strconv.Quote(req.Pattern))
	if err == nil {
		var result *zoekt.SearchResult
		result, err = index.Search(context.Background(), q, &zoekt.SearchOptions{
			ShardMaxMatchCount: 1000000000, TotalMaxMatchCount: 1000000000,
			MaxWallTime: 120 * time.Second,
		})
		if err == nil && result.Crashes != 0 {
			err = fmt.Errorf("incomplete search: %d crashes", result.Crashes)
		}
		if err == nil {
			for _, file := range result.Files {
				reply.Paths = append(reply.Paths, file.FileName)
			}
		}
	}
	reply.Duration = float64(time.Since(started).Nanoseconds()) / 1e6
	if err != nil {
		reply.Type = "Error"
		reply.Error = err.Error()
	}
	payload, err = json.Marshal(reply)
	if err != nil {
		return
	}
	if err = binary.Write(connection, binary.LittleEndian, uint32(len(payload))); err != nil {
		return
	}
	connection.Write(payload)
}

func main() {
	directory := flag.String("index", "", "index directory")
	path := flag.String("socket", "", "new Unix socket path")
	flag.Parse()
	index, err := search.NewDirectorySearcher(*directory)
	if err != nil {
		log.Fatal(err)
	}
	defer index.Close()
	listener, err := net.Listen("unix", *path)
	if err != nil {
		log.Fatal(err)
	}
	defer listener.Close()
	for {
		connection, err := listener.Accept()
		if err != nil {
			log.Fatal(err)
		}
		go serve(connection, index)
	}
}
