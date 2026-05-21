int s(int x) { return x + 1; }
int main(void) {
  return s(s(s(s(s(0)))));
}
