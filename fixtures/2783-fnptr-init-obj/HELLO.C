int handler(int x);
int (*op)(int) = handler;
int main(void) {
  return op(7);
}
