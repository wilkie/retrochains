int b[3];
int f(int *a) {
  return a[1];
}
int main() {
  b[1] = 7;
  return f(b);
}
