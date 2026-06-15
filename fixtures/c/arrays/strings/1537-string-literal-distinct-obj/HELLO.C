int f(char *p) {
  return *p;
}
int main(void) {
  return f("Hi") + f("Bye");
}
