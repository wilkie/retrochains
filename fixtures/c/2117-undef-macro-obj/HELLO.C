#define X 10
int main(void) {
  int a = X;
#undef X
#define X 99
  int b = X;
  return a + b;
}
