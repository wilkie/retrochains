int dbl(int x) { return x * 2; }
struct Op {
  int (*f)(int);
  int arg;
};
int main(void) {
  struct Op o;
  o.f = dbl;
  o.arg = 7;
  return o.f(o.arg);
}
