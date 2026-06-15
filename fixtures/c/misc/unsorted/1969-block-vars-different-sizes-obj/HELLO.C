int main(void) {
  int sum = 0;
  { long a = 100L; sum += (int)a; }
  { int b = 50; sum += b; }
  return sum;
}
