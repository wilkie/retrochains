int f(int x);
int g(int x);

int driver(int x) {
  return f(g(x));
}
