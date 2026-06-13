int a(int x) {
  return x + 1;
}
int b(int x) {
  return a(x) + 1;
}
int c(int x) {
  return b(x) + 1;
}
int main(void) {
  return c(5);
}
