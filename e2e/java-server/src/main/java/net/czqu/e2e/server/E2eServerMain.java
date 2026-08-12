package net.czqu.e2e.server;

import net.czqu.rmtt.api.AuthResult;
import net.czqu.rmtt.server.netty.RmttServer;
import net.czqu.rmtt.server.netty.RmttServerBuilder;

import java.nio.charset.StandardCharsets;

/**
 * Java server for the cross-language E2E matrix: a real {@link RmttServer}
 * that the Go / Rust rmtt clients connect to. Every upstream PUSH is echoed
 * back to the same device as {@code echo:<text>}.
 *
 * <p>Prints {@code E2E_SERVER_READY port=<port>} once bound.</p>
 */
public final class E2eServerMain {

    /** Default listening port when no argument is supplied. */
    public static final int DEFAULT_PORT = 19990;

    private E2eServerMain() {
    }

    /**
     * Start the E2E server and block forever.
     *
     * @param args optional single argument: the TCP port to listen on
     * @throws Exception when the server fails to start
     */
    public static void main(String[] args) throws Exception {
        int port = args.length > 0 ? Integer.parseInt(args[0]) : DEFAULT_PORT;
        final RmttServer[] serverRef = new RmttServer[1];
        RmttServer server = new RmttServerBuilder()
                .port(port)
                .authenticator(credential -> AuthResult.allow(credential))
                .messageHandler((deviceId, payload) -> {
                    String text = new String(payload, StandardCharsets.UTF_8);
                    System.out.println("PUSH_FROM_CLIENT device=" + deviceId + " payload=" + text);
                    System.out.flush();
                    serverRef[0].push(deviceId, ("echo:" + text).getBytes(StandardCharsets.UTF_8));
                })
                .build();
        serverRef[0] = server;
        server.awaitStartup();
        System.out.println("E2E_SERVER_READY port=" + port);
        System.out.flush();
        // keep the process alive until the harness kills it
        Thread.currentThread().join();
    }
}
