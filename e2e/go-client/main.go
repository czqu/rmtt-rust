// Command go-client is the Go side of the cross-language E2E check: an
// rmtt-go client against the Java rmtt server (e2e/java-server). It connects,
// pushes several messages upstream concurrently and verifies the server's
// echoes come back downstream, proving bidirectional interoperability.
//
// Prints GO_E2E_PASS on success; exits non-zero on any failure.
package main

import (
	"fmt"
	"os"
	"sync"
	"time"

	"github.com/czqu/rmtt-go/client"
)

func main() {
	if len(os.Args) < 2 {
		fmt.Fprintln(os.Stderr, "usage: go-client <port>")
		os.Exit(2)
	}
	addr := "tcp://127.0.0.1:" + os.Args[1]

	opts := client.NewClientOptions()
	opts.AddServer(addr)
	opts.SetCredential("go-e2e")
	opts.SetConnectTimeout(3 * time.Second)
	opts.AutoReconnect = false

	c := client.NewClient(opts)

	echo := make(chan []byte, 16)
	c.AddPayloadHandlerLast(func(_ client.Client, m client.Message) {
		echo <- m.Payload()
	})

	if t := c.Connect(); t.Wait() && t.Error() != nil {
		fmt.Fprintln(os.Stderr, "connect failed:", t.Error())
		os.Exit(1)
	}
	fmt.Println("GO_CONNECTED")

	// concurrent upstream pushes; the Java server echoes each one downstream
	const n = 5
	var wg sync.WaitGroup
	var pushErr error
	var errMu sync.Mutex
	for i := 0; i < n; i++ {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			if t := c.Push(fmt.Sprintf("ping-%d", i)); t.WaitTimeout(3*time.Second) && t.Error() != nil {
				errMu.Lock()
				pushErr = fmt.Errorf("push %d failed: %w", i, t.Error())
				errMu.Unlock()
			}
		}(i)
	}
	wg.Wait()
	if pushErr != nil {
		fmt.Fprintln(os.Stderr, pushErr)
		os.Exit(1)
	}

	// verify every echo arrives downstream
	want := make(map[string]bool, n)
	for i := 0; i < n; i++ {
		want[fmt.Sprintf("echo:ping-%d", i)] = true
	}
	for i := 0; i < n; i++ {
		select {
		case p := <-echo:
			delete(want, string(p))
		case <-time.After(5 * time.Second):
			fmt.Fprintf(os.Stderr, "timeout waiting for echo %d of %d\n", i, n)
			os.Exit(1)
		}
	}
	if len(want) != 0 {
		fmt.Fprintf(os.Stderr, "missing echoes: %v\n", want)
		os.Exit(1)
	}

	fmt.Printf("GO_ECHO_OK (%d concurrent echoes verified)\n", n)
	c.Disconnect(100)
	fmt.Println("GO_E2E_PASS")
	os.Exit(0)
}
