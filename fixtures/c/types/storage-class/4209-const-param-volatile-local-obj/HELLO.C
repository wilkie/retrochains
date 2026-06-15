int scale(const int factor)
{
  volatile int acc;

  acc = factor;
  acc = acc + factor;
  return acc;
}

int main(void)
{
  return scale(7);
}
