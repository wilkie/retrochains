int pascal inner(int x) { return x + 1; }
int pascal outer(int y) { return inner(y) * 2; }
int main(void) {
  return outer(10);
}
