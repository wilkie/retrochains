int dec(int);
int f(int c) { if (c > 0) return c; dec(c); return c; }
