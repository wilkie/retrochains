int add5(int x) { return x + 5; }
struct Op {
  int (*fn)(int);
};
int main(void) {
  struct Op op;
  op.fn = add5;
  return op.fn(10);
}
