// A pure byte relay: stdin to a TCP connection, the connection to stdout. It reads and writes
// nothing of its own, as an exec'd relay inside the server's container would.
// Usage: node embed-relay.mjs <host> <port>
import net from 'net';

const [host, port] = process.argv.slice(2);
const sock = net.connect(Number(port), host);
process.stdin.pipe(sock);
sock.pipe(process.stdout);
// An empty write's callback runs once every earlier write has been flushed.
sock.on('close', () => process.stdout.write('', () => process.exit(0)));
sock.on('error', () => process.exit(1));
process.stdin.on('end', () => sock.end());
