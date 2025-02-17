package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"strings"
	"time"

	"github.com/estuary/flow/go/bindings"
	pc "github.com/estuary/flow/go/protocols/capture"
	pfc "github.com/estuary/flow/go/protocols/capture"
	pf "github.com/estuary/flow/go/protocols/flow"
	"github.com/estuary/flow/go/protocols/ops"
	pr "github.com/estuary/flow/go/protocols/runtime"
	"github.com/gogo/protobuf/jsonpb"
	"github.com/sirupsen/logrus"
	pb "go.gazette.dev/core/broker/protocol"
	mbp "go.gazette.dev/core/mainboilerplate"
	"gopkg.in/yaml.v3"
)

type apiDiscover struct {
	Log         mbp.LogConfig         `group:"Logging" namespace:"log" env-namespace:"LOG"`
	Diagnostics mbp.DiagnosticsConfig `group:"Debug" namespace:"debug" env-namespace:"DEBUG"`
	Image       string                `long:"image" required:"true" description:"Docker image of the connector to use"`
	Network     string                `long:"network" description:"The Docker network that connector containers are given access to."`
	Name        string                `long:"name" description:"The Docker container name."`
	Config      string                `long:"config" description:"Path to the connector endpoint configuration"`
	Output      string                `long:"output" choice:"json" choice:"proto" default:"json"`
}

func (cmd apiDiscover) execute(ctx context.Context) (*pc.Response_Discovered, error) {
	var config, err = readConfig(cmd.Config)
	if err != nil {
		return nil, err
	}

	spec, err := json.Marshal(struct {
		Image  string          `json:"image"`
		Config json.RawMessage `json:"config"`
	}{
		Image:  cmd.Image,
		Config: config,
	})
	if err != nil {
		return nil, err
	}

	svc, err := bindings.NewTaskService(
		pr.TaskServiceConfig{
			TaskName:         cmd.Name,
			ContainerNetwork: cmd.Network,
			AllowLocal:       false, // TODO(johnny)?
		},
		ops.NewLocalPublisher(ops.ShardLabeling{TaskName: cmd.Name}),
	)
	if err != nil {
		return nil, fmt.Errorf("failed to create task service: %w", err)
	}
	defer svc.Drop()

	stream, err := pfc.NewConnectorClient(svc.Conn()).Capture(ctx)
	if err != nil {
		return nil, fmt.Errorf("starting capture: %w", err)
	}
	stream.Send(&pfc.Request{
		Discover: &pc.Request_Discover{
			ConnectorType: pf.CaptureSpec_IMAGE,
			ConfigJson:    spec,
		},
	})
	stream.CloseSend()

	response, err := stream.Recv()
	if err != nil {
		return nil, err
	}
	return response.Discovered, nil
}

func (cmd apiDiscover) Execute(_ []string) error {
	defer mbp.InitDiagnosticsAndRecover(cmd.Diagnostics)()
	mbp.InitLog(cmd.Log)

	var timeout = time.Second * 30
	if timeoutStr := os.Getenv("FLOW_DISCOVER_TIMEOUT"); timeoutStr != "" {
		if to, err := time.ParseDuration(timeoutStr); err != nil {
			return fmt.Errorf("failed to parse FLOW_DISCOVER_TIMEOUT value: %w", err)
		} else {
			timeout = to
		}
	}

	// Temporary exceptions for connectors that are known to have very slow discover
	// operations.
	// TODO(johnny): Allow larger timeouts across the board, after resolving
	// progress and UX issues of long-running discover operations.
	for _, image := range []string{
		"ghcr.io/estuary/source-salesforce",
		"ghcr.io/estuary/source-netsuite",
		"ghcr.io/estuary/source-redshift",
	} {
		if strings.HasPrefix(cmd.Image, image) && timeout < (time.Minute*5) {
			timeout = time.Minute * 5
		}
	}

	var ctx, cancelFn = context.WithTimeout(context.Background(), timeout)
	defer cancelFn()

	logrus.WithFields(logrus.Fields{
		"config":          cmd,
		"version":         mbp.Version,
		"buildDate":       mbp.BuildDate,
		"discoverTimeout": timeout,
	}).Debug("flowctl configuration")
	pb.RegisterGRPCDispatcher("local")

	var resp, err = cmd.execute(ctx)

	if errors.Is(err, context.DeadlineExceeded) {
		err = fmt.Errorf("Timeout while communicating with the endpoint. Please verify any address or firewall settings.")
	}
	if err != nil {
		fmt.Println(err.Error()) // Write to stdout so the agent can map into a draft error.
		return err
	}

	if cmd.Output == "json" {
		return (&jsonpb.Marshaler{}).Marshal(os.Stdout, resp)
	} else if cmd.Output == "proto" {
		var b, err = resp.Marshal()
		if err != nil {
			return err
		}
		_, err = os.Stdout.Write(b)
		return err
	} else {
		panic(cmd.Output)
	}
}

func readConfig(path string) (raw json.RawMessage, err error) {
	var iface interface{}

	if r, err := os.Open(path); err != nil {
		return nil, fmt.Errorf("opening config: %w", err)
	} else if err = yaml.NewDecoder(r).Decode(&iface); err != nil {
		return nil, fmt.Errorf("decoding config: %w", err)
	} else if raw, err = json.Marshal(iface); err != nil {
		return nil, fmt.Errorf("encoding JSON config: %w", err)
	}

	return raw, nil
}
