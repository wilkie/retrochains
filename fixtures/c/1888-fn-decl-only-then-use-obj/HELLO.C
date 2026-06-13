int callee(int x);
int main(void) {
  return callee(7);
}
int callee(int x) {
  return x * 2;
}
