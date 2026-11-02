// One frontend, two webviews: the account sidebar, and the title bar strip.
if (location.hash === '#titlebar') import('./titlebar.js');
else import('./sidebar.js');
