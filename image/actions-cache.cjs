// Loaded into each node action the runner starts, by the runner's wrapped node (node.sh, build-cache.sh,
// docs/adr/0082). The runner gives each node action GitHub's results service as ACTIONS_RESULTS_URL, after the step's
// own environment; this puts runnerd's in its place, at this container's address, before the action's code reads
// it. @actions/cache and BuildKit then save and restore on the host; every other request of the results service,
// such as an artifact's, runnerd sends to GitHub as it came. It changes nothing else, and nothing when the URL is not
// GitHub's.
'use strict';
(function () {
    var github = 'https://results-receiver.actions.githubusercontent.com/';
    var env = process.env;
    var url = env.ACTIONS_RESULTS_URL;
    if (!env.RUNNERD_ACTIONS_CACHE_PORT || !env.ACTIONS_CACHE_SERVICE_V2 || !url) return;
    if (url.replace(/\/*$/, '/') !== github) return;
    var interfaces = require('os').networkInterfaces();
    var address;
    Object.keys(interfaces).sort().forEach(function (name) {
        if (address || !/^eth\d+$/.test(name)) return;
        interfaces[name].forEach(function (net) {
            if (!address && (net.family === 'IPv4' || net.family === 4) && !net.internal) address = net.address;
        });
    });
    if (address) env.ACTIONS_RESULTS_URL = 'http://' + address + ':' + env.RUNNERD_ACTIONS_CACHE_PORT + '/';
})();
