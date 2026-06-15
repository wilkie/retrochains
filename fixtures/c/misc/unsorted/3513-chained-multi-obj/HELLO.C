int f(void);

int driver(void) {
  int x, y;
  x = y = f();
  return x + y;
}
