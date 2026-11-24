// Stands in for the server binary under Node: answers /healthz on --addr/--port, and stops on
// SIGTERM.
import { createServer } from 'node:http';

const flag = (name) =>
	process.argv
		.find((a) => a.startsWith(`--${name}=`))
		?.slice(name.length + 3);
const server = createServer((req, res) => {
	res.writeHead(req.url === '/healthz' ? 200 : 404).end('ok');
});
server.listen(Number(flag('port')), flag('addr'), () => {
	console.error(`listening on ${flag('addr')}:${flag('port')}`);
});
process.on('SIGTERM', () => process.exit(0));
