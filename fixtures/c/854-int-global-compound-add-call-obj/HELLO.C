int g;
int f() { return 5; }
int main() {
  g = 100;
  g += f();
  return 0;
}
