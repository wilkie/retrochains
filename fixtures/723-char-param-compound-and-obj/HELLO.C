int f(char c) {
  c &= 15;
  return c;
}
int main() {
  return f(63);
}
