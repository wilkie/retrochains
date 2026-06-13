int g(void);
int f(int x);

int driver(void) {
  return f(g() + 1);
}
